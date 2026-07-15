//! Reply-thread reconstruction from the session's row buffer.
//!
//! Neither Twitch nor Kick exposes a "fetch a thread by id" API, so a thread is
//! rebuilt purely from the messages still in the channel's ring buffer
//! (`MAX_ROWS`). Every reply carries its thread-root id (Twitch's
//! `reply-thread-parent-msg-id`; Kick's flat replies reuse the parent id), and
//! the root message's own id *is* that thread id — so grouping is a single pass:
//! collect every message whose [`Message::thread_id`] matches, in buffer order
//! (which is chronological). Messages that scrolled out of the buffer, or predate
//! the session, simply won't appear — the same limitation the web clients have
//! when you weren't watching.

use bks_core::Message;
use std::sync::Arc;

/// A reconstructed reply thread: the ordered chain of messages sharing one thread
/// root, oldest first. Always contains at least the seed message.
pub struct Thread {
    /// The thread's root id (the id every member's `thread_id()` shares). Read in
    /// tests; kept as the thread's identity for future callers (e.g. dedup).
    #[allow(dead_code)]
    pub root_id: String,
    /// The messages in the thread, oldest first.
    pub messages: Vec<Arc<Message>>,
}

impl Thread {
    /// The number of messages in the thread.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether this is a real conversation (more than the seed message alone).
    pub fn is_multi(&self) -> bool {
        self.messages.len() > 1
    }
}

/// Rebuilds the thread that the message with `seed_id` belongs to, from the given
/// messages (buffer order = chronological). Returns `None` if no message with
/// that id is present.
///
/// The seed's [`Message::thread_id`] fixes the thread; every message on the same
/// platform sharing it (including the root itself, whose `thread_id` is its own
/// id) is a member. `messages` should be the channel's rows in buffer order; the
/// result preserves that order.
pub fn reconstruct<'a>(
    messages: impl Iterator<Item = &'a Arc<Message>>,
    seed_id: &str,
) -> Option<Thread> {
    // Two-pass: buffer the messages once (the iterator is single-use), find the
    // seed to learn the thread id + platform, then collect the members.
    let all: Vec<&Arc<Message>> = messages.collect();
    let seed = all.iter().find(|m| m.id == seed_id)?;
    let thread_id = seed.thread_id().to_string();
    let platform = seed.platform;

    let members: Vec<Arc<Message>> = all
        .iter()
        .filter(|m| m.platform == platform && m.thread_id() == thread_id)
        .map(|m| (*m).clone())
        .collect();

    Some(Thread {
        root_id: thread_id,
        messages: members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bks_core::{Author, Color, MessageElement, Platform, ReplyParent};
    use chrono::Utc;

    fn msg(id: &str, reply_to: Option<(&str, &str)>) -> Arc<Message> {
        // `reply_to` = (parent_id, thread_root_id).
        let reply = reply_to.map(|(pid, root)| ReplyParent {
            author: "someone".into(),
            text: "parent".into(),
            parent_id: Some(pid.into()),
            thread_root_id: Some(root.into()),
        });
        Arc::new(Message {
            id: id.into(),
            platform: Platform::Twitch,
            channel: "chan".into(),
            timestamp: Utc::now(),
            author: Author {
                login: "u".into(),
                display_name: "U".into(),
                color: Color::from_hex("#ffffff"),
                badges: vec![],
                user_id: "1".into(),
                paint: None,
            },
            elements: vec![MessageElement::Text {
                text: id.into(),
                color: None,
            }],
            raw_text: id.into(),
            reply,
            first_message: false,
            highlighted: false,
            historical: false,
            reward_id: None,
        })
    }

    #[test]
    fn single_message_is_its_own_thread() {
        let rows = vec![msg("a", None)];
        let t = reconstruct(rows.iter(), "a").unwrap();
        assert_eq!(t.root_id, "a");
        assert_eq!(t.len(), 1);
        assert!(!t.is_multi());
    }

    #[test]
    fn collects_whole_chain_from_any_member() {
        // root "a"; "b" replies to a (thread root a); "c" replies to b (root a);
        // "x" is unrelated.
        let rows = vec![
            msg("a", None),
            msg("x", None),
            msg("b", Some(("a", "a"))),
            msg("c", Some(("b", "a"))),
        ];
        // Seeding from any member yields the full ordered chain a,b,c (not x).
        for seed in ["a", "b", "c"] {
            let t = reconstruct(rows.iter(), seed).unwrap();
            assert_eq!(t.root_id, "a");
            let ids: Vec<&str> = t.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["a", "b", "c"]);
            assert!(t.is_multi());
        }
    }

    #[test]
    fn unrelated_reply_is_a_separate_thread() {
        let rows = vec![
            msg("a", None),
            msg("b", Some(("a", "a"))),
            msg("p", None),
            msg("q", Some(("p", "p"))),
        ];
        let t = reconstruct(rows.iter(), "q").unwrap();
        assert_eq!(t.root_id, "p");
        let ids: Vec<&str> = t.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["p", "q"]);
    }

    #[test]
    fn missing_seed_returns_none() {
        let rows = vec![msg("a", None)];
        assert!(reconstruct(rows.iter(), "zzz").is_none());
    }

    #[test]
    fn root_scrolled_out_still_groups_replies() {
        // If the root "a" was trimmed from the buffer, its replies still share the
        // thread id and group together (seeded from either reply).
        let rows = vec![msg("b", Some(("a", "a"))), msg("c", Some(("b", "a")))];
        let t = reconstruct(rows.iter(), "c").unwrap();
        assert_eq!(t.root_id, "a");
        let ids: Vec<&str> = t.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c"]);
    }
}
