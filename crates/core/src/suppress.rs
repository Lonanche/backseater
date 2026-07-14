//! User-defined suppress list: messages matching any entry stay in the feed but
//! are rendered at very low contrast (dimmed but readable), so the eye skips
//! them while they remain available. The middle tier between "show" and
//! [`IgnoreList`](crate::IgnoreList) (which hides entirely).
//!
//! Same entry grammar as the ignore list (see `term_rules`): a plain phrase,
//! `re:<regex>`, or `user:[platform/]<name>` to dim everything a user sends.
//! Unlike ignore, suppression is **always** a per-view render
//! concern — a suppressed message must still render, so it is never dropped at
//! ingest.

use crate::message::Message;
use crate::term_rules::TermRules;

/// A compiled set of suppress rules. Cheap to clone (regexes are `Arc`-backed
/// internally); rebuild it when the settings list changes.
#[derive(Clone, Default)]
pub struct SuppressList(TermRules);

impl SuppressList {
    /// Builds the list from raw settings entries. Blank entries, invalid
    /// regexes, and malformed user rules are skipped.
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        Self(TermRules::new(entries))
    }

    /// Whether the message matches any suppress rule — its text against the
    /// phrase/regex rules, or its author against the `user:` rules — so it
    /// should be dimmed.
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
    use super::SuppressList;
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
        let l = SuppressList::new(["buy now".to_string(), "user:streamelements".to_string()]);
        assert!(l.matches_message(&msg(Platform::Twitch, "someone", "hey BUY NOW cheap")));
        assert!(l.matches_message(&msg(Platform::Twitch, "StreamElements", "hello")));
        assert!(l.matches_message(&msg(Platform::Kick, "StreamElements", "hello")));
        assert!(!l.matches_message(&msg(Platform::Kick, "someone", "hello")));
    }
}
