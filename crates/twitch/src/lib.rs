//! Twitch connector: live read via anonymous IRC (`tmi`), authenticated send via
//! IRC, and moderation via Helix REST ([`TwitchActions`]).

mod actions;
mod badges;
mod builder;
mod connector;
mod eventsub;
mod eventsub_manager;
mod helix;
mod history;
mod http;
mod irc_manager;
mod ivr;
mod preview;
mod pubsub;
mod videos;
mod viewers;

pub use actions::{TwitchActions, TwitchUserCard};
pub use badges::{fetch_badges, BadgeMap};
pub use connector::{TwitchAuth, TwitchSource};
pub use eventsub::EventsubAuth;
pub use eventsub_manager::{register as register_eventsub, Registration as EventsubRegistration};
pub use helix::{fetch_pinned_message, Chatter, Chatters, UserInfo};
pub use history::fetch_recent;
pub use ivr::{fetch_live_status, LiveStatus, SubAge};
pub use preview::TwitchClipPreviewProvider;
pub use pubsub::run as run_pubsub;
pub use videos::fetch_last_stream;
pub use viewers::fetch_viewer_count;

// Re-exported for unit testing the parser in isolation.
pub use builder::build_privmsg_elements;
