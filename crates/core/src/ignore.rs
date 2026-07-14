//! User-defined ignore list: messages matching any entry are hidden from chat.
//! The inverse of [`MentionMatcher`](crate::MentionMatcher) — same "build from
//! settings, test per message" shape, but it filters instead of highlighting.
//!
//! The entry grammar (shared with [`SuppressList`](crate::SuppressList) — see
//! `term_rules`): a plain phrase (case-insensitive substring of the text),
//! `re:<regex>`, or `user:[platform/]<name>` to hide everything a user sends
//! (optionally on one platform only).

use crate::message::Message;
use crate::term_rules::TermRules;

/// A compiled set of ignore rules. Cheap to clone (regexes are `Arc`-backed
/// internally); rebuild it when the settings list changes.
#[derive(Clone, Default)]
pub struct IgnoreList(TermRules);

impl IgnoreList {
    /// Builds the list from raw settings entries. Blank entries, invalid
    /// regexes, and malformed user rules are skipped (so a typo can't crash or
    /// silently swallow everything).
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        Self(TermRules::new(entries))
    }

    /// Whether the message matches any ignore rule — its text against the
    /// phrase/regex rules, or its author against the `user:` rules — so it
    /// should be hidden.
    pub fn matches_message(&self, msg: &Message) -> bool {
        self.0.matches_message(msg)
    }

    /// Whether the list has no rules (so the UI can skip building/applying it).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::IgnoreList;
    use crate::message::{Author, Message, Platform};

    fn msg(platform: Platform, login: &str, text: &str) -> Message {
        Message {
            id: String::new(),
            platform,
            channel: String::new(),
            timestamp: chrono::Utc::now(),
            author: Author {
                login: login.into(),
                display_name: login.into(),
                ..Default::default()
            },
            elements: Vec::new(),
            raw_text: text.into(),
            reply: None,
            first_message: false,
            highlighted: false,
            historical: false,
            reward_id: None,
        }
    }

    #[test]
    fn matches_message_on_text_or_author() {
        let l = IgnoreList::new(["spam".to_string(), "user:kick/kickbot".to_string()]);
        assert!(l.matches_message(&msg(Platform::Twitch, "someone", "this is SPAM")));
        assert!(l.matches_message(&msg(Platform::Kick, "KickBot", "hello")));
        assert!(!l.matches_message(&msg(Platform::Twitch, "KickBot", "hello")));
        assert!(!l.matches_message(&msg(Platform::Kick, "someone", "hello")));
    }
}
