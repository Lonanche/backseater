use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::emote::Emote;

/// Which service a message came from. The UI uses this for accent colors and
/// platform indicators; connectors set it once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    Twitch,
    Kick,
    YouTube,
    TikTok,
}

impl Platform {
    /// A short label for the platform, e.g. for tooltips or the channel header.
    pub fn label(self) -> &'static str {
        match self {
            Platform::Twitch => "Twitch",
            Platform::Kick => "Kick",
            Platform::YouTube => "YouTube",
            Platform::TikTok => "TikTok",
        }
    }

    /// The platform's logo image, shown as an icon before each message — an app
    /// **asset path** (served by the UI's `AssetSource`, a small bundled raster),
    /// not a URL. `None` for platforms without one yet — the UI falls back to
    /// [`glyph`](Self::glyph). We bundle a small PNG rather than fetch the logo SVG:
    /// gpui rasterizes an `img()` SVG at its huge intrinsic size (the Twitch logo →
    /// ~105 MB decoded for a 16px icon). YouTube/TikTok have no bundled icon yet.
    pub fn icon_url(self) -> Option<&'static str> {
        match self {
            Platform::Twitch => Some("twitch/twitch.png"),
            Platform::Kick => Some("kick/kick.png"),
            Platform::YouTube => Some("youtube/youtube.png"),
            Platform::TikTok => None,
        }
    }

    /// A single-character glyph marking a message's source. Used as the icon when
    /// [`icon_url`](Self::icon_url) has no logo. Kept here (not in the UI) so each
    /// platform defines its own indicator.
    pub fn glyph(self) -> &'static str {
        match self {
            Platform::Twitch => "T",
            Platform::Kick => "K",
            Platform::YouTube => "Y",
            Platform::TikTok => "♪",
        }
    }

    /// The platform's brand/accent color, used to tint its glyph.
    pub fn color(self) -> Color {
        match self {
            Platform::Twitch => Color::rgb(0x91, 0x47, 0xff),
            Platform::Kick => Color::rgb(0x53, 0xfc, 0x18),
            Platform::YouTube => Color::rgb(0xff, 0x00, 0x00),
            Platform::TikTok => Color::rgb(0x69, 0xc9, 0xd0),
        }
    }

    /// The channel's page URL (for a link/tooltip click). Twitch/Kick channels
    /// are plain logins/slugs; a YouTube source can be a handle, bare name, `UC…`
    /// id, or a pasted URL — normalized here.
    pub fn channel_url(self, channel: &str) -> String {
        let c = channel.trim().trim_start_matches('#');
        match self {
            Platform::Twitch => format!("https://www.twitch.tv/{c}"),
            Platform::Kick => format!("https://kick.com/{c}"),
            Platform::YouTube => {
                if c.contains("youtube.com") || c.contains("youtu.be") {
                    if c.starts_with("http") {
                        c.to_string()
                    } else {
                        format!("https://{c}")
                    }
                } else if c.starts_with("UC") && c.len() == 24 {
                    format!("https://www.youtube.com/channel/{c}")
                } else {
                    format!("https://www.youtube.com/@{}", c.trim_start_matches('@'))
                }
            }
            Platform::TikTok => format!("https://www.tiktok.com/@{c}"),
        }
    }
}

/// A plain RGB color, decoupled from any GUI type. The UI converts this into
/// its own color representation when rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Packs the color into a `0xRRGGBB` integer, as GUI color helpers expect.
    pub const fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | self.b as u32
    }

    /// Parses a `#RRGGBB` hex string (as Twitch sends in the `color` tag).
    pub fn from_hex(s: &str) -> Option<Self> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if s.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some(Self { r, g, b })
    }
}

/// A small image shown next to a username (moderator, subscriber, 7TV, ...).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Badge {
    pub id: String,
    pub url: String,
    /// Human-readable name shown on hover (e.g. "Subscriber", "Moderator", "VIP").
    /// Filled by the platform that knows it (Twitch); `None` omits the tooltip.
    #[serde(default)]
    pub title: Option<String>,
}

/// One step of a paint gradient: a position in `0.0..=1.0` along the gradient
/// and the packed `0xRRGGBB` color at that position (alpha is dropped — text is
/// drawn opaque).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaintStop {
    pub at: f32,
    pub color: u32,
}

/// How a [`NamePaint`] colors a username.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PaintKind {
    /// A single flat color (or a URL/image paint we can't render as a gradient,
    /// collapsed to its representative color).
    Solid(u32),
    /// A linear gradient at `angle` degrees across the name's width.
    Linear { angle: f32, stops: Vec<PaintStop> },
    /// A radial gradient from the name's center outward.
    Radial { stops: Vec<PaintStop> },
}

/// A 7TV "paint" — a cosmetic that recolors a username with a gradient or solid
/// color. Resolved from 7TV's cosmetics API and rendered by the UI as a gradient
/// (or flat) fill over the name. Platform-agnostic so the UI never touches 7TV.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamePaint {
    /// The paint's name, shown on hover.
    pub name: String,
    pub kind: PaintKind,
}

/// The author of a message, normalized across platforms.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Author {
    pub login: String,
    pub display_name: String,
    pub color: Option<Color>,
    pub badges: Vec<Badge>,
    /// A 7TV paint coloring this author's name, when they have one and the
    /// feature is enabled. Filled by the bridge after an async cosmetics lookup;
    /// `None` otherwise (the name uses its plain color).
    #[serde(default)]
    pub paint: Option<NamePaint>,
    /// Platform numeric user id, when the connector provides it. Used to resolve
    /// moderation targets on platforms (e.g. Kick) whose API can't look up a
    /// user by name — we remember ids from chatters we've seen. Empty if unknown.
    #[serde(default)]
    pub user_id: String,
}

/// One renderable token of a message. The UI maps each variant to an element;
/// this is the seam between connectors and rendering. Adding inline content
/// later (cheers, replies, ...) means adding a variant here.
///
/// `Emote` holds an [`Arc`] so the same emote (a global/channel set is shared
/// across every tab and re-resolved into every matching message) is interned —
/// cloning an element is a pointer bump, not a copy of its three `String`s. This
/// matters because the virtualized log re-renders visible rows every frame and
/// each on-screen emote clones itself into a click closure per frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MessageElement {
    Text { text: String, color: Option<Color> },
    Emote(std::sync::Arc<Emote>),
    Badge(Badge),
    Mention { login: String },
    Link { url: String, text: String },
}

pub type ChannelId = String;

/// The message a reply points at, shown as a muted "replying to" line above the
/// message body. Both Twitch (IRC `reply-parent-*` tags) and Kick (`metadata.
/// original_*`) provide the parent's author + body, so we keep just those — no
/// thread model, just the one-line context the UI renders.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplyParent {
    /// Display name of the user being replied to.
    pub author: String,
    /// The parent message's text (already stripped of any reply mention prefix).
    pub text: String,
}

/// A single chat message in platform-agnostic form.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub platform: Platform,
    pub channel: ChannelId,
    pub timestamp: DateTime<Utc>,
    pub author: Author,
    /// The renderable token stream (text runs, emotes, ...).
    pub elements: Vec<MessageElement>,
    /// Plain text of the whole message, kept for search and logging.
    pub raw_text: String,
    /// Set when this message is a reply: the parent's author + body for the
    /// "replying to" context line. `None` for normal messages.
    #[serde(default)]
    pub reply: Option<ReplyParent>,
    /// The author's first-ever message in this channel (Twitch `first-msg`
    /// tag). The UI tints its background like Twitch's "first time
    /// chatting" highlight. Only Twitch sets this; false everywhere else.
    #[serde(default)]
    pub first_message: bool,
    /// A backfilled message from chat history (loaded on channel join), not a
    /// live one. The UI renders these faded to set them apart.
    #[serde(default)]
    pub historical: bool,
}
