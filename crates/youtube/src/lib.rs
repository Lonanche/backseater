//! YouTube connector: anonymous InnerTube live-chat reads ([`YouTubeSource`]).
//!
//! Reads run in-process with no API key, OAuth, or quota (YouTube's private web
//! "InnerTube" API — the same one youtube.com uses). Sending / moderation go
//! through the quota-limited official Data API v3 with a Google login
//! ([`YoutubeActions`]).

mod actions;
mod api;
mod builder;
mod connector;
mod preview;
mod resolve;
mod seen;
mod streams;

pub use actions::{AuthExpired, OnRefreshed, YoutubeActions, MAX_MESSAGE_CHARS};
pub use connector::YouTubeSource;
pub use preview::YoutubePreviewProvider;
pub use seen::{author_roles, channel_id_for, AuthorRoles};

// Re-exported for unit testing the resolver + renderer parser in isolation.
pub use builder::build_item;
pub use resolve::{extract_video_id, resolve_live_video_id};
