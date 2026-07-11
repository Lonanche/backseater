//! YouTube connector: anonymous InnerTube live-chat reads ([`YouTubeSource`]).
//!
//! Reads run in-process with no API key, OAuth, or quota (YouTube's private web
//! "InnerTube" API — the same one youtube.com uses). Sending / moderation (which
//! need the quota-limited Data API v3 + Google OAuth) are a later phase.

mod api;
mod builder;
mod connector;
mod resolve;
mod streams;

pub use connector::YouTubeSource;

// Re-exported for unit testing the resolver + renderer parser in isolation.
pub use builder::build_item;
pub use resolve::{extract_video_id, resolve_live_video_id};
