//! Reply-thread reconstruction from the session's row buffer.
//!
//! Neither Twitch nor Kick exposes a "fetch a thread by id" API, so a thread is
//! rebuilt purely from the messages still in the channel's ring buffer
//! (`MAX_ROWS`). Messages that scrolled out of the buffer, or predate the session,
//! simply won't appear — the same limitation the web clients have when you weren't
//! watching.
//!
//! ⚠️ We do **not** trust the platform's thread-root field. Twitch's
//! `reply-thread-parent-msg-id` *is* a stable root, but Kick's `thread_parent_id`
//! only points one level up (at the direct parent) past the first reply — so
//! grouping by [`Message::thread_id`] fragments a deep Kick chain into
//! parent-sized pieces (verified against a live 4-message chain). Instead we walk
//! the **direct-parent** links ([`Message::parent_id`], reliable on both
//! platforms) up to the deepest ancestor still in the buffer, and treat *that* as
//! the thread's root — so `a←b←c←d` all resolve to `a` no matter what each
//! reply's root field says.

use bks_core::Message;
use std::collections::HashMap;
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

/// Resolves the true thread root of `msg` by walking its **direct-parent** links
/// (`parent_id`) up through `by_id` — the buffered messages keyed by id — until it
/// reaches a message with no parent, or one whose parent isn't in the buffer. The
/// deepest reachable ancestor's id is the root. A cycle guard (unreachable with
/// real ids, but cheap) caps the walk at the buffer size.
fn resolve_root(msg: &Arc<Message>, by_id: &HashMap<&str, &Arc<Message>>) -> String {
    let mut current = msg;
    let mut steps = 0;
    while let Some(pid) = current.parent_id() {
        match by_id.get(pid) {
            Some(parent) if steps < by_id.len() => {
                current = parent;
                steps += 1;
            }
            // Parent scrolled out of the buffer: this parent id is the furthest
            // ancestor we can name, so it's the root of the visible chain.
            _ => return pid.to_string(),
        }
    }
    // `current` is a non-reply (or its parent left the buffer above): it's the root.
    current.id.clone()
}

/// Rebuilds the thread that the message with `seed_id` belongs to, from the given
/// messages (buffer order = chronological). Returns `None` if no message with that
/// id is present.
///
/// Members are grouped by their **resolved root** ([`resolve_root`], which walks
/// direct-parent links) rather than the platform's thread-root field — so a deep
/// chain stays one thread even when that field is unreliable (Kick). `messages`
/// should be the channel's rows in buffer order; the result preserves that order.
pub fn reconstruct<'a>(
    messages: impl Iterator<Item = &'a Arc<Message>>,
    seed_id: &str,
) -> Option<Thread> {
    // Buffer the messages once (the iterator is single-use) + index by id for the
    // parent-link walk.
    let all: Vec<&Arc<Message>> = messages.collect();
    let by_id: HashMap<&str, &Arc<Message>> =
        all.iter().map(|m| (m.id.as_str(), *m)).collect();

    let seed = *by_id.get(seed_id)?;
    let platform = seed.platform;
    let root_id = resolve_root(seed, &by_id);

    let members: Vec<Arc<Message>> = all
        .iter()
        .filter(|m| m.platform == platform && resolve_root(m, &by_id) == root_id)
        .map(|m| (*m).clone())
        .collect();

    Some(Thread {
        root_id,
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

    #[test]
    fn deep_chain_groups_despite_unreliable_root_field() {
        // Reproduces a real Kick capture: `thread_root_id` (from Kick's
        // `thread_parent_id`) points at the DIRECT PARENT, not the true root, past
        // the first level — so grouping by it would split a←b←c←d into {a,b} and
        // {c,d}. Walking parent_id links must still yield one thread rooted at a.
        //   a (root)  b→a(root a)  c→b(root *b*)  d→c(root *b*)
        let rows = vec![
            msg("a", None),
            msg("b", Some(("a", "a"))),
            msg("c", Some(("b", "b"))), // wrong root field (should be a)
            msg("d", Some(("c", "b"))), // wrong root field (should be a)
        ];
        for seed in ["a", "b", "c", "d"] {
            let t = reconstruct(rows.iter(), seed).unwrap();
            assert_eq!(t.root_id, "a", "seed {seed} should resolve root a");
            let ids: Vec<&str> = t.messages.iter().map(|m| m.id.as_str()).collect();
            assert_eq!(ids, vec!["a", "b", "c", "d"], "seed {seed}");
        }
    }

    #[test]
    fn parent_cycle_is_bounded() {
        // Defensive: a (corrupt) parent cycle must not loop forever.
        let rows = vec![msg("x", Some(("y", "y"))), msg("y", Some(("x", "x")))];
        // Just needs to terminate and return *something* without hanging.
        let t = reconstruct(rows.iter(), "x").unwrap();
        assert!(!t.messages.is_empty());
    }
}
