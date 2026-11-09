//! Process-wide memory of what the anonymous InnerTube reader has seen, for the
//! Data API actions ([`crate::YoutubeActions`]):
//!
//! - the live video each source is currently on (so sending skips re-resolving);
//! - recent messages' author + timestamp, because the Data API's message ids
//!   differ from InnerTube's — a delete re-finds the message in
//!   `liveChatMessages.list` by author + time;
//! - chatter login → channel id (bans key on the `UC…` id), and each author's
//!   moderator/owner role (the app's YouTube mod-status signal).
//!
//! Bounded: the oldest messages fall out past [`MAX_MESSAGES`].

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;

const MAX_MESSAGES: usize = 20_000;

/// What a delete needs to re-find a message through the Data API.
#[derive(Clone, Debug)]
pub struct SeenMessage {
    pub author_channel_id: String,
    pub timestamp: DateTime<Utc>,
    pub text: String,
}

/// A chat author's standing in one channel's chat.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthorRoles {
    pub moderator: bool,
    pub owner: bool,
}

#[derive(Default)]
struct Seen {
    live_videos: HashMap<String, String>,
    messages: HashMap<String, SeenMessage>,
    order: VecDeque<String>,
    logins: HashMap<String, String>,
    /// The reverse of `logins` (channel id → login), for ban actions the
    /// reader sees, which name only the channel id.
    names: HashMap<String, String>,
    /// Keyed by (source, author channel id): a mod in one chat isn't in another.
    roles: HashMap<(String, String), AuthorRoles>,
}

static SEEN: Lazy<Mutex<Seen>> = Lazy::new(Mutex::default);

pub(crate) fn set_live_video(source: &str, video_id: Option<&str>) {
    let mut seen = SEEN.lock().unwrap();
    match video_id {
        Some(id) => seen.live_videos.insert(source.to_string(), id.to_string()),
        None => seen.live_videos.remove(source),
    };
}

/// The live video the reader is currently following for `source`, if any.
pub fn live_video(source: &str) -> Option<String> {
    SEEN.lock().unwrap().live_videos.get(source).cloned()
}

pub(crate) fn record(msg: &bks_core::Message, roles: AuthorRoles) {
    if msg.id.is_empty() || msg.author.user_id.is_empty() {
        return;
    }
    let mut seen = SEEN.lock().unwrap();
    let seen = &mut *seen;
    if !msg.author.login.is_empty() {
        let login = msg.author.login.to_lowercase();
        seen.names.insert(msg.author.user_id.clone(), login.clone());
        seen.logins.insert(login, msg.author.user_id.clone());
    }
    seen.roles
        .insert((msg.channel.clone(), msg.author.user_id.clone()), roles);
    let entry = SeenMessage {
        author_channel_id: msg.author.user_id.clone(),
        timestamp: msg.timestamp,
        text: msg.raw_text.clone(),
    };
    if seen.messages.insert(msg.id.clone(), entry).is_none() {
        seen.order.push_back(msg.id.clone());
        while seen.order.len() > MAX_MESSAGES {
            if let Some(old) = seen.order.pop_front() {
                seen.messages.remove(&old);
            }
        }
    }
}

pub fn message(id: &str) -> Option<SeenMessage> {
    SEEN.lock().unwrap().messages.get(id).cloned()
}

/// The channel id of a chatter we've seen, by login (case-insensitive). A raw
/// `UC…` channel id is accepted as-is.
pub fn channel_id_for(user: &str) -> Option<String> {
    let user = user.trim().trim_start_matches('@');
    if is_channel_id(user) {
        return Some(user.to_string());
    }
    SEEN.lock()
        .unwrap()
        .logins
        .get(&user.to_lowercase())
        .cloned()
}

/// The login of a chatter we've seen, by channel id.
pub(crate) fn login_for(channel_id: &str) -> Option<String> {
    SEEN.lock().unwrap().names.get(channel_id).cloned()
}

/// The last-seen roles of the author with this channel id in `source`'s chat.
pub fn author_roles(source: &str, channel_id: &str) -> Option<AuthorRoles> {
    SEEN.lock()
        .unwrap()
        .roles
        .get(&(source.to_string(), channel_id.to_string()))
        .copied()
}

fn is_channel_id(s: &str) -> bool {
    s.len() == 24
        && s.starts_with("UC")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_raw_channel_ids() {
        let id = "UCabcdefghijklmnopqrstuv";
        assert_eq!(channel_id_for(id).as_deref(), Some(id));
        assert_eq!(channel_id_for("nobody-seen-yet"), None);
    }
}
