//! Chat search: its own OS window (like the viewer list) listing the tab's
//! buffered chat history in chat order, filtered live by a search box; clicking
//! a result jumps the tab's log to that message (the mention-jump flash).
//!
//! State lives on the [`ChatView`](crate::chatview) (which owns the hosting
//! child window and the virtualized result list); this module holds the pure
//! matching/filtering used by the reconcile (kept here so it's unit-testable).

use std::sync::Arc;

use bks_core::{contains_ci, Message};

use crate::chatview::Row;

/// Normalizes a query for matching — trimmed + lowercased once up front, so
/// the per-message tests don't re-normalize. All matching below expects it.
/// Deliberately does NOT strip a leading `@` (unlike mention terms): chat text
/// legitimately contains literal `@name` pings worth searching for.
pub fn normalize(query: &str) -> String {
    query.trim().to_lowercase()
}

/// Whether `msg` matches an already-[`normalize`]d query: case-insensitive
/// substring of the message text only (usernames are deliberately not
/// searched); empty matches all. This runs per buffered message on every
/// rebuild — each keystroke and each buffer change while the search window is
/// open — so the matching is the allocation-free [`bks_core::contains_ci`].
pub fn matches(msg: &Message, query: &str) -> bool {
    query.is_empty() || contains_ci(&msg.raw_text, query)
}

/// The buffered chat messages matching an already-[`normalize`]d query, in the
/// order `rows` yields them (pass the buffer in chat order). Only plain chat
/// rows are searched: event/system/error rows aren't messages the log can jump
/// to.
pub fn filter<'a>(rows: impl Iterator<Item = &'a Row>, query: &str) -> Vec<&'a Arc<Message>> {
    rows.filter_map(|row| match row {
        Row::Message { msg } => matches(msg, query).then_some(msg),
        _ => None,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::{filter, normalize};
    use crate::chatview::Row;
    use bks_core::{Author, Message, Platform};
    use chrono::Utc;
    use std::sync::Arc;

    fn message_row(id: &str, login: &str, name: &str, text: &str) -> Row {
        Row::Message {
            msg: Arc::new(Message {
                id: id.into(),
                platform: Platform::Twitch,
                channel: "chan".into(),
                timestamp: Utc::now(),
                author: Author {
                    login: login.into(),
                    display_name: name.into(),
                    color: None,
                    badges: vec![],
                    user_id: "1".into(),
                    paint: None,
                },
                elements: vec![],
                raw_text: text.into(),
                reply: None,
                first_message: false,
                highlighted: false,
                historical: false,
                reward_id: None,
            }),
        }
    }

    #[test]
    fn empty_query_matches_all_messages_only() {
        let rows = [
            message_row("1", "alice", "Alice", "hello there"),
            Row::System("notice".into()),
            message_row("2", "bob", "Bob", "hi"),
        ];
        assert_eq!(filter(rows.iter(), &normalize("")).len(), 2);
        assert_eq!(filter(rows.iter(), &normalize("   ")).len(), 2);
    }

    #[test]
    fn matches_text_case_insensitively_but_not_usernames() {
        let rows = [
            message_row("1", "alice", "Alice", "Hello WORLD"),
            message_row("2", "bob", "ボブ", "kappa"),
        ];
        assert_eq!(filter(rows.iter(), &normalize("world"))[0].id, "1");
        assert_eq!(filter(rows.iter(), &normalize("ボ")).len(), 0);
        assert!(filter(rows.iter(), &normalize("ALI")).is_empty());
        assert!(filter(rows.iter(), &normalize("bob")).is_empty());
        assert!(filter(rows.iter(), &normalize("zzz")).is_empty());
    }

    #[test]
    fn ascii_path_handles_mixed_case_and_multibyte_haystacks() {
        let rows = [message_row("1", "alice", "Alice", "ボブ says KAPPA")];
        // ASCII needle against a multibyte haystack (the allocation-free path).
        assert_eq!(filter(rows.iter(), &normalize("kApPa")).len(), 1);
        assert!(filter(rows.iter(), &normalize("kappaz")).is_empty());
    }

    #[test]
    fn preserves_iteration_order() {
        let rows = [
            message_row("old", "a", "A", "x"),
            message_row("new", "b", "B", "x"),
        ];
        let in_order = filter(rows.iter(), &normalize("x"));
        assert_eq!(in_order[0].id, "old");
        assert_eq!(in_order[1].id, "new");
    }
}
