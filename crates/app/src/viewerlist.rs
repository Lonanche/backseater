//! The Twitch viewer list: its own OS window (like the usercard) listing who is
//! connected to the tab's Twitch chat, with a search filter.
//!
//! Twitch only exposes this to the broadcaster and moderators (Helix
//! `GET /chat/chatters`); the anonymous list on twitch.tv rides its
//! browser-integrity-gated GQL, which third-party clients can't use — so a
//! non-mod gets an explanatory error instead of names. Kick has no chatters
//! API at all, so the list is Twitch-only.
//!
//! State lives on the [`ChatView`](crate::chatview) (which owns the fetch and
//! the hosting child window); this module holds the list's own data + the pure
//! filtering used by the render (kept here so it's unit-testable).

use bks_twitch::{Chatter, Chatters};

/// Async state of the viewer-list fetch.
pub enum State {
    /// Fetch in flight (initial open or a refresh).
    Loading,
    /// The list arrived; chatters are sorted by login for display.
    Loaded(Chatters),
    /// Twitch refused or the request failed; shown in place of the list.
    Failed(String),
}

/// One open viewer list (per tab; a re-open refreshes it in the same window).
pub struct ViewerList {
    /// The Twitch channel the list is for (the tab's channel at open time).
    pub channel: String,
    pub state: State,
}

impl ViewerList {
    pub fn new(channel: String) -> Self {
        Self {
            channel,
            state: State::Loading,
        }
    }

    /// Stores a fetch result, sorting the names for display.
    pub fn resolve(&mut self, result: anyhow::Result<Chatters>) {
        self.state = match result {
            Ok(mut chatters) => {
                chatters
                    .chatters
                    .sort_by(|a, b| a.user_login.cmp(&b.user_login));
                State::Loaded(chatters)
            }
            Err(err) => State::Failed(format!("{err:#}")),
        };
    }
}

/// How many names the window renders at once. The body isn't virtualized, so a
/// huge channel (tens of thousands of chatters) is capped and the footer says
/// how many more the search can narrow down to.
pub const MAX_SHOWN: usize = 500;

/// The chatters matching `query` (case-insensitive substring of login or
/// display name; an empty query matches all), in the stored (sorted) order.
/// Matching is the shared allocation-free [`bks_core::contains_ci`].
pub fn filter<'a>(chatters: &'a [Chatter], query: &str) -> Vec<&'a Chatter> {
    let query = query.trim().to_lowercase();
    chatters
        .iter()
        .filter(|c| {
            query.is_empty()
                || bks_core::contains_ci(&c.user_login, &query)
                || bks_core::contains_ci(&c.user_name, &query)
        })
        .collect()
}

/// The display label for one chatter: the display name, with the login in
/// parentheses when it differs (localized names).
pub fn label(chatter: &Chatter) -> String {
    if chatter.user_name.is_empty() {
        return chatter.user_login.clone();
    }
    if chatter.user_name.to_lowercase() == chatter.user_login {
        chatter.user_name.clone()
    } else {
        format!("{} ({})", chatter.user_name, chatter.user_login)
    }
}

#[cfg(test)]
mod tests {
    use super::{filter, label};
    use bks_twitch::Chatter;

    fn chatter(login: &str, name: &str) -> Chatter {
        Chatter {
            user_id: "1".into(),
            user_login: login.into(),
            user_name: name.into(),
        }
    }

    #[test]
    fn empty_query_matches_all() {
        let all = [chatter("alice", "Alice"), chatter("bob", "Bob")];
        assert_eq!(filter(&all, "").len(), 2);
        assert_eq!(filter(&all, "   ").len(), 2);
    }

    #[test]
    fn matches_login_and_display_name_case_insensitively() {
        let all = [chatter("alice", "Alice"), chatter("bob", "ボブ")];
        assert_eq!(filter(&all, "ALI").len(), 1);
        assert_eq!(filter(&all, "ボ").len(), 1);
        assert_eq!(filter(&all, "zzz").len(), 0);
    }

    #[test]
    fn label_shows_login_only_when_it_differs() {
        assert_eq!(label(&chatter("alice", "Alice")), "Alice");
        assert_eq!(label(&chatter("bob", "ボブ")), "ボブ (bob)");
        assert_eq!(label(&chatter("carol", "")), "carol");
    }
}
