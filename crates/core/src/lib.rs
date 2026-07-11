//! Platform-agnostic domain model shared by every connector and the UI.
//!
//! Nothing in this crate depends on a GUI framework or on networking. Adding a
//! new platform means producing [`Message`] values; the UI only ever reads them.

mod emote;
mod ignore;
mod links;
mod mention;
mod message;
mod scale;
mod text;
mod theme;
mod time;

pub use emote::{Emote, EmoteTooltip};
pub use ignore::IgnoreList;
pub use links::linkify;
pub use mention::MentionMatcher;
pub use message::{
    Author, Badge, ChannelId, Color, Message, MessageElement, NamePaint, PaintKind, PaintStop,
    Platform, ReplyParent,
};
pub use scale::{preferred_scale, set_preferred_scale};
pub use text::{channel_login, encode_url_component, plural, strip_channel};
pub use theme::{is_dark_theme, set_dark_theme};
pub use time::{parse_rfc3339, reconnect_delay};
