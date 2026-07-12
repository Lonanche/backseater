//! Kick connector: anonymous Pusher WebSocket chat ([`KickSource`]) for reading,
//! plus authenticated REST actions ([`KickActions`]) for sending + moderation.

mod actions;
mod api;
mod builder;
mod connector;
mod history;

pub use actions::{AuthExpired, KickActions, OnRefreshed, PinnableMessage};
pub use api::{
    fetch_channel_emotes, fetch_channel_info, fetch_user_info, fetch_viewer_count, slugify,
    ChannelInfo, KickUserInfo, LastStream, SubscriberBadge,
};
pub use connector::KickSource;
pub use history::fetch_recent;

// Re-exported for unit testing the inline-emote parser in isolation.
pub use builder::parse_content;
