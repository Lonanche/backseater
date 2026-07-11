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
    /// "replying to" context line.
    reply: u32,
    /// Default username color when a chatter has none.
    default_name: u32,
    /// Background tint for a message that mentions the user.
    mention_bg: u32,
    /// Background tint + label color for a chatter's first message.
    first_message_bg: u32,
    first_message_label: u32,
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
    /// Selected-tab chip background — matches `chat_bg` so the active tab reads as
    /// the content surface it sits above, instead of the kit's
    /// near-black `background` which looked like a hole.
    tab_active_bg: u32,
    /// Unselected-tab chip background (a recessed tone vs the active tab).
    tab_inactive_bg: u32,
    /// Floating-panel surface (settings/usercard windows) — an *elevated* tone so a
    /// panel reads as raised above the app, not the kit's flat near-black.
    panel_bg: u32,
}

/// The dark palette (the original look).
const DARK: Palette = Palette {
    chat_bg: 0x1a1a1d,
    link: 0x4a9eff,
    timestamp: 0x8a8a8a,
    system: 0x6a9955,
    reply: 0x8a8a8a,
    default_name: 0x9147ff,
    mention_bg: 0x3a2e1a,
    first_message_bg: 0x1c3a36,
    first_message_label: 0xc792ea,
    event_bg: 0x322a44,
    event_text: 0xefe9ff,
    streak_bg: 0x3a3320,
    streak_text: 0xf6e7b0,
    live_bg: 0x1c3a24,
    live_text: 0x8af2a8,
    offline_bg: 0x2a2a2e,
    offline_text: 0x9a9a9a,
    error_bg: 0x3a1f22,
    error_text: 0xff8a8a,
    automod_bg: 0x3a2a1c,
    automod_text: 0xf2c078,
    automod_allow: 0x8af2a8,
    automod_deny: 0xff8a8a,
    tab_bar_bg: 0x101013,
    tab_active_bg: 0x1a1a1d,
    tab_inactive_bg: 0x141417,
    panel_bg: 0x202024,
};

/// The light palette (a brighter look).
const LIGHT: Palette = Palette {
    chat_bg: 0xf7f7f8,
    link: 0x1f6feb,
    timestamp: 0x6e6e74,
    system: 0x2f7d32,
    reply: 0x6e6e74,
    default_name: 0x772ce8,
    mention_bg: 0xfff3d1,
    first_message_bg: 0xd6f3ec,
    first_message_label: 0x8250df,
    event_bg: 0xece5ff,
    event_text: 0x432c91,
    streak_bg: 0xfff1c9,
    streak_text: 0x7a5a12,
    live_bg: 0xd9f5e1,
    live_text: 0x1c6b32,
    offline_bg: 0xececed,
    offline_text: 0x67676d,
    error_bg: 0xfde3e3,
    error_text: 0xb42318,
    automod_bg: 0xfdeed3,
    automod_text: 0x8a5a10,
    automod_allow: 0x1c6b32,
    automod_deny: 0xb42318,
    tab_bar_bg: 0xe6e6ea,
    tab_active_bg: 0xf7f7f8,
    tab_inactive_bg: 0xeaeaee,
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
            first_message_bg: c.first_message,
            first_message_label: fg(c.first_message, c.first_message),
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
            tab_active_bg: c.chat_bg,
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

/// The selected-tab chip background — matches [`chat_bg`] so the active tab reads
/// as the content surface below it.
pub fn tab_active_bg() -> u32 {
    palette().tab_active_bg
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

/// Minimum contrast ratio (WCAG-style, 1..21) a username must clear against the
/// chat background. Below it the name is moved away from the background (lightened
/// on a dark bg, darkened on a light bg) until it passes — BTTV's
/// "readable colors". 2.0 keeps even fairly low-contrast hues legible without
/// washing colors out the way the AA text threshold (4.5) would.
const MIN_NAME_CONTRAST: f32 = 2.0;

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

/// GPUI's default text line-height factor: `phi()`, the golden ratio (see
/// `TextStyleRefinement::default` in gpui's `style.rs`). A text token's element
/// height equals exactly one line box (`round(font_size × this)`; see gpui's
/// `TextLayout::layout`). Every row item — text *and* the icon/time/badges — is
/// placed in a box of this height so they share the same box top in the
/// `items_start` row; images are then offset inside it to the text's *optical*
/// center (see [`image_line_box`]).
const LINE_HEIGHT: f32 = 1.618_034;

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
        // gpui rounds the line box to whole pixels (`line_height_in_pixels`);
        // match it so the prefix boxes are exactly one text line tall.
        let line = (font_size * LINE_HEIGHT).round();
        let ascent = font_metric(&FONT_ASCENT) * font_size;
        let descent = font_metric(&FONT_DESCENT) * font_size;
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
    match platform.icon_url() {
        Some(url) => image_line_box(scale, scale.icon).child(
            img(SharedString::from(url))
                .id(SharedString::from(platform.label()))
                .h(px(scale.icon))
                .w(px(scale.icon)),
        ),
        None => line_box(scale).child(
            div()
                .px_1()
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
const HISTORY_OPACITY: f32 = 0.6;

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
fn push_run(
    tokens: &mut Vec<gpui::AnyElement>,
    ctx: &RenderCtx,
    ordinal: &mut usize,
    text: &str,
    color: Option<u32>,
    seventv_link_click: Option<&SeventvLinkClick>,
    link_hover: Option<&LinkHover>,
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
            push_link(tokens, ctx, ordinal, word, word, link_hover);
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

    match click {
        None => base.child(inner).into_any_element(),
        Some(click) => {
            let sel = ctx.selection.clone();
            // Underline via a bottom border, not text-decoration: a gradient name is
            // a row of per-character boxes, and an inherited `underline()` draws under
            // each box separately, leaving gaps between letters. A border on this
            // wrapper spans the whole name as one unbroken line. Colored to match the
            // name (the base color for a gradient), transparent until hovered.
            base.id(ctx.ids.token(click_ord))
                .cursor_pointer()
                .border_b_1()
                .border_color(gpui::transparent_black())
                .hover(move |s| s.border_color(rgb(color)))
                .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                    if !sel.has_selection() {
                        click(window, cx);
                    }
                })
                .child(inner)
                .into_any_element()
        }
    }
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
        tokens.push(
            div()
                .id(ctx.ids.token(ord))
                .text_color(rgb(palette().link))
                .cursor_pointer()
                .when(hovered, |s| s.underline())
                .on_hover(move |entered, _window, cx| {
                    let id = entered.then_some(link_id);
                    if hover_sel.set_hovered_link(id) {
                        if let Some(cb) = &on_hover {
                            cb(cx); // ask the view to repaint with the new state
                        }
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

/// A callback run when a row's reply button is clicked: starts a reply to that
/// message. Built per-row by the view, capturing the message's reply identity.
/// `None` outside the live log (and on non-message rows).
pub type ReplyClick = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// A callback run when a row's pin button is clicked: pins that message to the
/// top of its platform's chat. Built per-row by the view, only when the
/// logged-in user can moderate the row's platform; `None` hides the button.
pub type PinClick = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// The shared shape of a row's hover-action callbacks ([`ReplyClick`],
/// [`PinClick`]), for the chip builder.
type RowAction = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// The per-row interaction callbacks the view supplies (none in contexts like the
/// usercard list, where rows aren't interactive). `name_click` opens the clicked
/// chatter's usercard; `link_hover` repaints so a wrapped link underlines as one;
/// `emote_click` opens an emote-info popup; `reply_click` starts a reply to the row.
#[derive(Default)]
pub struct RowHandlers {
    pub name_click: Option<NameClick>,
    pub link_hover: Option<LinkHover>,
    pub emote_click: Option<EmoteClick>,
    pub seventv_link_click: Option<SeventvLinkClick>,
    pub reply_click: Option<ReplyClick>,
    /// Set only when the logged-in user can moderate this row's platform — the
    /// pin button renders (on hover) only then.
    pub pin_click: Option<PinClick>,
}

/// Per-message display flags, set by the view from the row's state.
#[derive(Clone, Copy, Default)]
pub struct RowFlags {
    /// The author was banned/timed-out or the message deleted: strike + fade it.
    pub struck: bool,
    /// The message mentions the user: tint its background.
    pub mentioned: bool,
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
        link_hover,
        emote_click,
        seventv_link_click,
        reply_click,
        pin_click,
    } = handlers;
    let RowFlags { struck, mentioned } = flags;
    let scale = Scale::new(font_size);
    let name_color = readable_color(
        msg.author
            .color
            .map(Color::to_u32)
            .unwrap_or_else(|| palette().default_name),
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
    ));

    // Twitch prepends `@ParentName ` to a reply's body (Kick doesn't). With the
    // "replying to" line shown above, that prefix is redundant — strip it from
    // the first text run. Matched against the known parent author so we only ever
    // remove the actual reply mention, never text the user typed.
    let reply_prefix = msg.reply.as_ref().map(|r| r.author.as_str());
    let mut first_text = true;

    let mut emote_index = 0usize;
    for element in &msg.elements {
        match element {
            MessageElement::Text { text, color } => {
                let shown = if first_text {
                    strip_reply_prefix(text, reply_prefix)
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
                tokens.push(text_token(
                    &ctx,
                    ordinal,
                    format!("@{login}"),
                    None,
                    true,
                    false,
                ));
            }
            MessageElement::Link { url, text } => {
                push_link(&mut tokens, &ctx, ordinal, url, text, link_hover.as_ref());
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

    // No row gap: body words carry their own whitespace (so copy is exact and
    // words wrap individually). The structural prefix — icon, time, badges, name
    // — gets explicit right margins instead, and emotes their own.
    let body = h_flex()
        .w_full()
        .min_w_0()
        .flex_wrap()
        // Top-align: when the message wraps to several lines, centering would push
        // the first lines above the name/badges — start-align keeps the name on the
        // first line with text flowing down beneath it.
        .items_start()
        // Banned/timed-out: strike through the whole row and fade it.
        .when(struck, |row| row.line_through().opacity(STRUCK_OPACITY))
        .child(platform_badge(msg.platform, scale).mr_1())
        // The timestamp is chat-font-sized: full-size text fills the line box, so it shares the
        // body text's baseline exactly — a smaller size can't be made to align
        // with both the text baseline and the icon/badge centers at once.
        .child(
            line_box(scale)
                .mr_1()
                .text_color(rgb(palette().timestamp))
                .child(time),
        )
        .children(author_badges)
        .children(tokens);

    // A first-time chatter's row gets a "FIRST MESSAGE" label pinned to the
    // top-right corner (Twitch-style). Wrap the body so the label sits
    // beside it without being caught in the body's flex-wrap.
    let body = if msg.first_message {
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .child(div().flex_1().min_w_0().child(body))
            .child(
                div()
                    .flex_none()
                    .ml_2()
                    .text_size(px(scale.small))
                    .text_color(rgb(palette().first_message_label))
                    .child("FIRST MESSAGE"),
            )
            .into_any_element()
    } else {
        body.into_any_element()
    };

    // Hover actions (📌 pin for moderators, ↩ reply) pinned to the row's right
    // edge, hidden until the row is hovered (group-hover fades them in).
    // `flex_none` keeps them from being squeezed; the group id ties the hover to
    // this row only. Kept in the layout always (just transparent) so their
    // appearance doesn't reflow the row.
    // The row's hover group is named by the base id itself (group names only
    // have to be distinct from other group names, and nothing else groups on
    // message ids) — like the ids above, a refcount bump instead of a per-frame
    // `format!`.
    let group_id = ids.base.clone();
    let body = if reply_click.is_some() || pin_click.is_some() {
        let mut actions = h_flex().flex_none().items_start();
        if let Some(cb) = pin_click {
            actions = actions.child(hover_action(ids.pin(), "📌 pin", &group_id, scale, cb));
        }
        if let Some(cb) = reply_click {
            actions = actions.child(hover_action(ids.reply(), "↩ reply", &group_id, scale, cb));
        }
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .child(div().flex_1().min_w_0().child(body))
            .child(actions)
            .into_any_element()
    } else {
        body.into_any_element()
    };

    // A reply adds a muted context line above the message; without one we return
    // the row directly so non-reply messages keep their single-row layout. A
    // message that mentions the user, or a chatter's first message in the channel,
    // gets a tinted background — mention wins when a row is both.
    let row_bg = if mentioned {
        Some(palette().mention_bg)
    } else if msg.first_message {
        Some(palette().first_message_bg)
    } else {
        None
    };
    v_flex()
        .group(group_id)
        .w_full()
        .min_w_0()
        // Backfilled history is dimmed to set it apart from live chat.
        .when(msg.historical, |row| row.opacity(HISTORY_OPACITY))
        // The tinted pill gets horizontal padding for the rounded look, with an
        // equal negative margin so the message content stays flush with normal
        // (un-highlighted) rows instead of being nudged right — the tint floats as
        // a pill that bleeds an equal amount past the text on each side.
        .when_some(row_bg, |row, bg| {
            row.bg(rgb(bg))
                .rounded_md()
                .px(px(HIGHLIGHT_INSET))
                .mx(px(-HIGHLIGHT_INSET))
        })
        .when_some(msg.reply.as_ref(), |col, reply| {
            col.child(reply_line(reply, scale))
        })
        .child(body)
}

/// One hover-revealed action chip at a row's right edge ("↩ reply", "📌 pin"):
/// transparent until the row (its `group_id`) is hovered, highlighted on its own
/// hover, firing `cb` on mouse-down (with propagation stopped so the same
/// mouse-down doesn't refocus the log and steal focus back from the input).
fn hover_action(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    group_id: &SharedString,
    scale: Scale,
    cb: RowAction,
) -> gpui::AnyElement {
    div()
        .id(id)
        .flex_none()
        .ml_1()
        .px_1()
        .cursor_pointer()
        .text_size(px(scale.small))
        .text_color(rgb(palette().timestamp))
        .opacity(0.)
        .group_hover(group_id.clone(), |s| s.opacity(1.))
        .hover(|s| s.text_color(rgb(palette().default_name)))
        .child(SharedString::from(label))
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
/// parent body truncated to [`REPLY_PREVIEW_CHARS`].
fn reply_line(reply: &ReplyParent, scale: Scale) -> impl IntoElement {
    let mut preview: String = reply.text.chars().take(REPLY_PREVIEW_CHARS).collect();
    if reply.text.chars().count() > REPLY_PREVIEW_CHARS {
        preview.push('…');
    }
    div()
        .text_size(px(scale.small))
        .text_color(rgb(palette().reply))
        .child(SharedString::from(format!(
            "↪ replying to @{}: {preview}",
            reply.author
        )))
}

/// A connector notice (connected, errors, ...), rendered muted.
pub fn render_system(text: &str, font_size: f32) -> impl IntoElement {
    div()
        .text_size(px(Scale::new(font_size).small))
        .text_color(rgb(palette().system))
        .child(SharedString::from(text.to_string()))
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

    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .my_px()
        // A floating pill: padding + equal negative margin so the ⚠ + text align
        // with normal rows while the tint bleeds an equal amount past it each side.
        .px(px(HIGHLIGHT_INSET))
        .mx(px(-HIGHLIGHT_INSET))
        .py_1()
        .rounded_md()
        .border_l_2()
        .border_color(rgb(p.error_text))
        .bg(rgb(p.error_bg))
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
        &format!("AutoMod held a message from {user} ({reason}):"),
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

    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .my_px()
        // The same floating-pill inset as error/event rows, so content aligns.
        .px(px(HIGHLIGHT_INSET))
        .mx(px(-HIGHLIGHT_INSET))
        .py_1()
        .rounded_md()
        .border_l_2()
        .border_color(rgb(p.automod_text))
        .bg(rgb(p.automod_bg))
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

/// A public channel event (sub/gift/raid/watch-streak): a highlighted row with
/// the platform icon and the ready-made event text. On a sub/resub the chatter's
/// attached chat message (`message`) is rendered *under* the system text as its
/// own chat-style line — timestamp, badges, colored name, body with emotes inline
/// — like twitch.tv shows a resub. `timestamp` (when given) prefixes the system
/// text; the events panel passes it, the chat log doesn't (the log's neighbors
/// carry the time). Watch-streak rows (`kind`) get a distinct amber tint. The
/// text/message column wraps; the icon pins to the top so it stays aligned with
/// the first line when the content spans several.
/// `flush` selects the pill's box model: `false` (the chat log) floats the tint
/// past the row box with a negative margin so the content lines up with normal
/// rows; `true` (the events panel) keeps the pill inside its row box — the
/// panel has no normal rows to align with, and the bleed interacts badly with
/// its container's edges (uneven left/right margins).
pub fn render_event(
    platform: Platform,
    kind: EventKind,
    text: &str,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
    message: Option<&Message>,
    font_size: f32,
    flush: bool,
) -> impl IntoElement {
    let scale = Scale::new(font_size);
    let p = palette();
    let (bg, fg) = match kind {
        EventKind::WatchStreak => (p.streak_bg, p.streak_text),
        _ => (p.event_bg, p.event_text),
    };

    // The system text is split into per-word tokens (like a chat body) so a long
    // event string wraps at word boundaries instead of overflowing the (often
    // narrow) row/panel. Each word keeps its trailing whitespace as the gap (no
    // margin — like chat body words; a margin on top of the space reads double
    // wide). A per-row id (hashed from the text) gives each emote a stable
    // element id so GPUI advances its animation frames.
    let row_id = stable_id(text);
    let tokens: Vec<gpui::AnyElement> = split_words(text)
        .into_iter()
        .filter(|word| !word.trim().is_empty())
        .map(|word| {
            div()
                .child(SharedString::from(word.to_string()))
                .into_any_element()
        })
        .collect();

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
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .items_center()
                .children(tokens),
        );

    v_flex()
        .w_full()
        .min_w_0()
        .my_px()
        // A floating pill: `px` padding for the rounded box + (in the log) an
        // equal negative margin so the icon/text line up with normal rows while
        // the tint bleeds an equal amount past the content on each side (and
        // trailing content clears the rounded corner instead of being clipped).
        .px(px(HIGHLIGHT_INSET))
        .when(!flush, |pill| pill.mx(px(-HIGHLIGHT_INSET)))
        .py_1()
        .rounded_md()
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .text_size(px(scale.font))
        .child(header)
        .when_some(message, |col, msg| {
            col.child(event_message_line(msg, scale, row_id))
        })
}

/// The chat line shown under a sub/resub's system text: the chatter's attached
/// message rendered like a normal chat row — timestamp, author badges, bold
/// colored name, then the body's words/emotes — but non-interactive, matching the
/// rest of the event pill. A solid 7TV paint colors the name; a gradient paint is
/// collapsed to its midpoint color (the full per-char gradient needs the
/// selectable-token machinery the event row doesn't carry).
fn event_message_line(msg: &Message, scale: Scale, row_id: u64) -> impl IntoElement {
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
            .unwrap_or(p.default_name),
    );
    let name_color = match msg.author.paint.as_ref().map(|paint| &paint.kind) {
        Some(PaintKind::Solid(c)) => *c,
        Some(PaintKind::Linear { stops, .. }) | Some(PaintKind::Radial { stops }) => {
            sample_gradient(stops, 0.5).unwrap_or(base_color)
        }
        None => base_color,
    };

    let badges: Vec<gpui::AnyElement> = msg
        .author
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
        .collect();

    h_flex()
        .w_full()
        .min_w_0()
        .flex_wrap()
        .items_start()
        .child(
            line_box(scale)
                .mr_1()
                .text_color(rgb(p.timestamp))
                .child(time),
        )
        .children(badges)
        .child(
            div()
                .mr_1()
                .font_weight(NAME_WEIGHT)
                .text_color(rgb(name_color))
                .child(SharedString::from(format!("{}:", msg.author.display_name))),
        )
        .children(inline_tokens(&msg.elements, scale, ("event-emote", row_id)))
}

/// Renders a message's token stream as non-interactive inline elements (words +
/// inline emote images), the shared core of the event row and the reply preview.
/// `seed` gives each emote a stable element id (so GPUI animates it); the per-row
/// part is the caller's, the index is added per emote here.
fn inline_tokens(
    message: &[MessageElement],
    scale: Scale,
    seed: (&'static str, u64),
) -> Vec<gpui::AnyElement> {
    let mut tokens: Vec<gpui::AnyElement> = Vec::new();
    let mut emote_index = 0u64;
    for element in message {
        match element {
            MessageElement::Text { text, .. } => {
                // Each word keeps its trailing whitespace as the inter-word gap,
                // like chat body words — no extra margin.
                for word in split_words(text) {
                    if word.trim().is_empty() {
                        continue;
                    }
                    tokens.push(
                        div()
                            .child(SharedString::from(word.to_string()))
                            .into_any_element(),
                    );
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
            // Mentions/links render as plain text; these previews aren't interactive.
            MessageElement::Mention { login } => {
                tokens.push(
                    div()
                        .mr_1()
                        .child(SharedString::from(format!("@{login}")))
                        .into_any_element(),
                );
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
        ))
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
    let p = palette();
    let (bg, fg) = if live {
        (p.live_bg, p.live_text)
    } else {
        (p.offline_bg, p.offline_text)
    };
    let text = if live {
        if title.trim().is_empty() {
            format!("● {} stream is live", platform.label())
        } else {
            format!("● {} stream is live — {title}", platform.label())
        }
    } else {
        format!("○ {} stream went offline", platform.label())
    };

    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .gap_2()
        .my_px()
        // A floating pill: padding + equal negative margin so content aligns with
        // normal rows while the tint bleeds an equal amount past it on each side.
        .px(px(HIGHLIGHT_INSET))
        .mx(px(-HIGHLIGHT_INSET))
        .py_1()
        .rounded_md()
        .bg(rgb(bg))
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
        break_long_word, contrast_ratio, emote_tooltip_text, lerp_color, readable_color_on,
        sample_gradient, seventv_emote_id, split_words, strip_reply_prefix, DARK, LIGHT,
        LONG_WORD_CHARS, MIN_NAME_CONTRAST,
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
