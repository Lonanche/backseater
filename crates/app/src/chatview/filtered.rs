use std::collections::VecDeque;
use std::sync::Arc;

use bks_core::Message;
use gpui::ListState;

use super::Row;
use crate::channel_store::ChannelEvent;

/// One membership bit per source row lets normal appends and front trims update
/// a filtered list without searching the retained history again.
#[derive(Default)]
pub(super) struct FilteredMessages {
    membership: VecDeque<bool>,
    pub rows: VecDeque<Arc<Message>>,
    generation: Option<u64>,
    #[cfg(test)]
    pub rebuilds: usize,
}

impl FilteredMessages {
    pub fn needs_rebuild(&self) -> bool {
        self.generation.is_none()
    }

    pub fn invalidate(&mut self) {
        self.generation = None;
    }

    pub fn is_current(&self, generation: u64) -> bool {
        self.generation == Some(generation)
    }

    pub fn rebuild(
        &mut self,
        rows: &VecDeque<Row>,
        generation: u64,
        list: &ListState,
        matches: impl Fn(bool, &Message) -> bool,
    ) {
        self.membership.clear();
        self.rows.clear();
        for row in rows {
            let msg = match row {
                Row::Message { msg } if matches(true, msg) => Some(msg.clone()),
                Row::Event {
                    message: Some(msg), ..
                } if matches(false, msg) => Some(Arc::new((**msg).clone())),
                _ => None,
            };
            self.membership.push_back(msg.is_some());
            if let Some(msg) = msg {
                self.rows.push_back(msg);
            }
        }
        list.reset(self.rows.len());
        self.generation = Some(generation);
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
    }

    pub fn apply(
        &mut self,
        event: &ChannelEvent,
        list: &ListState,
        matches: impl Fn(bool, &Message) -> bool,
    ) -> bool {
        let (generation, insertion) = match event {
            ChannelEvent::Appended {
                generation,
                index,
                msg,
                is_message,
                ..
            }
            | ChannelEvent::Inserted {
                generation,
                index,
                msg,
                is_message,
            } => (*generation, Some((*index, msg, *is_message))),
            ChannelEvent::RemovedFront { generation } => (*generation, None),
            _ => return false,
        };
        let Some(previous) = self.generation else {
            return false;
        };
        if generation <= previous {
            return false;
        }
        if generation != previous + 1 {
            self.invalidate();
            return false;
        }
        self.generation = Some(generation);
        if let Some((index, msg, is_message)) = insertion {
            if index > self.membership.len() {
                self.invalidate();
                return false;
            }
            let matched = msg.as_ref().filter(|msg| matches(is_message, msg));
            let result_index = matched.map(|_| {
                if index == self.membership.len() {
                    self.rows.len()
                } else {
                    self.membership
                        .iter()
                        .take(index)
                        .filter(|&&yes| yes)
                        .count()
                }
            });
            self.membership.insert(index, matched.is_some());
            if let (Some(msg), Some(ix)) = (matched, result_index) {
                self.rows.insert(ix, msg.clone());
                list.splice(ix..ix, 1);
                return true;
            }
        } else {
            match self.membership.pop_front() {
                Some(true) => {
                    self.rows.pop_front();
                    list.splice(0..1, 0);
                    return true;
                }
                Some(false) => {}
                None => self.invalidate(),
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bks_core::{Author, Platform};
    use gpui::{px, ListAlignment};
    use std::cell::Cell;

    fn message(id: &str, text: &str) -> Arc<Message> {
        Arc::new(Message {
            id: id.into(),
            platform: Platform::Twitch,
            channel: "fixture".into(),
            timestamp: chrono::Utc::now(),
            author: Author::default(),
            raw_text: text.into(),
            elements: vec![],
            reply: None,
            first_message: false,
            highlighted: false,
            historical: false,
            reward_id: None,
        })
    }

    fn list() -> ListState {
        ListState::new(0, ListAlignment::Bottom, px(200.))
    }

    fn appended(index: usize, generation: u64, msg: Option<Arc<Message>>) -> ChannelEvent {
        ChannelEvent::Appended {
            index,
            generation,
            is_message: msg.is_some(),
            msg,
            historical: false,
        }
    }

    fn ids(filtered: &FilteredMessages) -> Vec<&str> {
        filtered.rows.iter().map(|msg| msg.id.as_str()).collect()
    }

    #[test]
    fn appends_only_check_new_messages_and_front_trims_preserve_membership() {
        let mut filtered = FilteredMessages::default();
        let list = list();
        filtered.rebuild(&VecDeque::new(), 0, &list, |_, _| false);
        let checks = Cell::new(0);
        let matches = |_: bool, msg: &Message| {
            checks.set(checks.get() + 1);
            msg.raw_text.contains("match")
        };
        let a = message("a", "match");
        let b = message("b", "ordinary");
        let c = message("c", "match");
        assert!(!filtered.apply(&appended(0, 1, None), &list, matches));
        assert!(filtered.apply(&appended(1, 2, Some(a.clone())), &list, matches));
        assert!(!filtered.apply(&appended(2, 3, Some(b)), &list, matches));
        assert!(filtered.apply(&appended(3, 4, Some(c.clone())), &list, matches));
        assert_eq!(checks.get(), 3);
        assert_eq!(ids(&filtered), ["a", "c"]);
        assert!(Arc::ptr_eq(&filtered.rows[0], &a));
        assert_eq!(list.item_count(), 2);
        assert!(!filtered.apply(
            &ChannelEvent::RemovedFront { generation: 5 },
            &list,
            matches
        ));
        assert!(filtered.apply(
            &ChannelEvent::RemovedFront { generation: 6 },
            &list,
            matches
        ));
        assert!(!filtered.apply(
            &ChannelEvent::RemovedFront { generation: 7 },
            &list,
            matches
        ));
        assert_eq!(ids(&filtered), ["c"]);
        assert!(Arc::ptr_eq(&filtered.rows[0], &c));
        assert_eq!(list.item_count(), 1);
        assert_eq!(checks.get(), 3, "trims must not rerun filtering");
        assert_eq!(filtered.rebuilds, 1);
        assert!(filtered.is_current(7));
    }

    #[test]
    fn historical_insertions_keep_filtered_order_among_nonmatching_rows() {
        let a = message("a", "match");
        let z = message("z", "match");
        let mut filtered = FilteredMessages::default();
        let list = list();
        filtered.rebuild(
            &VecDeque::from([
                Row::System("notice".into()),
                Row::Message { msg: a },
                Row::Message {
                    msg: message("excluded", "other"),
                },
                Row::Message { msg: z },
            ]),
            10,
            &list,
            |_, msg| msg.raw_text == "match",
        );
        let mut historical = (*message("middle", "match")).clone();
        historical.historical = true;
        assert!(filtered.apply(
            &ChannelEvent::Inserted {
                index: 3,
                generation: 11,
                msg: Some(Arc::new(historical)),
                is_message: true,
            },
            &list,
            |_, msg| msg.raw_text == "match"
        ));
        assert!(!filtered.apply(
            &ChannelEvent::Inserted {
                index: 0,
                generation: 12,
                msg: None,
                is_message: false,
            },
            &list,
            |_, _| panic!("nonmessage row was filtered")
        ));
        assert_eq!(ids(&filtered), ["a", "middle", "z"]);
        assert_eq!(list.item_count(), 3);
        assert_eq!(filtered.rebuilds, 1);
    }

    #[test]
    fn event_attached_messages_use_the_same_policy_for_seed_and_updates() {
        let msg = message("event", "match");
        let source = VecDeque::from([Row::Event {
            platform: Platform::Twitch,
            kind: bks_platform::EventKind::Sub,
            text: "subscription".into(),
            timestamp: msg.timestamp,
            message: Some(Box::new((*msg).clone())),
            accent: None,
            actor: None,
            historical: false,
        }]);
        for include_events in [false, true] {
            let mut filtered = FilteredMessages::default();
            let list = list();
            let matches = |is_message: bool, _: &Message| is_message || include_events;
            filtered.rebuild(&source, 1, &list, matches);
            let extra = message("extra", "match");
            assert_eq!(
                filtered.apply(
                    &ChannelEvent::Appended {
                        index: 1,
                        generation: 2,
                        msg: Some(extra),
                        is_message: false,
                        historical: false,
                    },
                    &list,
                    matches
                ),
                include_events
            );
            assert_eq!(filtered.rows.len(), if include_events { 2 } else { 0 });
            assert_eq!(list.item_count(), filtered.rows.len());
        }
    }

    #[test]
    fn queued_events_already_covered_by_rebuild_do_not_duplicate_results() {
        let mut filtered = FilteredMessages::default();
        let list = list();
        filtered.rebuild(
            &VecDeque::from([Row::Message {
                msg: message("seed", "match"),
            }]),
            8,
            &list,
            |_, _| true,
        );
        for event in [
            appended(20, 8, Some(message("replayed", "match"))),
            ChannelEvent::RemovedFront { generation: 7 },
            ChannelEvent::Inserted {
                index: 0,
                generation: 6,
                msg: Some(message("history", "match")),
                is_message: true,
            },
        ] {
            assert!(!filtered.apply(&event, &list, |_, _| panic!(
                "seeded event was filtered again"
            )));
        }
        assert!(filtered.is_current(8));
        assert_eq!(ids(&filtered), ["seed"]);
        assert_eq!(list.item_count(), 1);
        assert!(filtered.apply(
            &appended(1, 9, Some(message("new", "match"))),
            &list,
            |_, _| true
        ));
        assert_eq!(ids(&filtered), ["seed", "new"]);
    }

    #[test]
    fn generation_gaps_and_invalid_indices_require_one_fresh_seed() {
        let mut filtered = FilteredMessages::default();
        let list = list();
        filtered.rebuild(&VecDeque::new(), 0, &list, |_, _| true);
        assert!(!filtered.apply(
            &appended(0, 2, Some(message("gap", "match"))),
            &list,
            |_, _| panic!("gap must invalidate before filtering")
        ));
        assert!(!filtered.is_current(2));
        assert!(!filtered.apply(
            &appended(0, 3, Some(message("later", "match"))),
            &list,
            |_, _| true
        ));
        assert_eq!(list.item_count(), 0);
        filtered.rebuild(
            &VecDeque::from([Row::Message {
                msg: message("seed", "match"),
            }]),
            3,
            &list,
            |_, _| true,
        );
        assert!(filtered.apply(
            &appended(1, 4, Some(message("next", "match"))),
            &list,
            |_, _| true
        ));
        assert_eq!(ids(&filtered), ["seed", "next"]);
        assert_eq!(filtered.rebuilds, 2);
        assert!(!filtered.apply(
            &appended(99, 5, Some(message("bad index", "match"))),
            &list,
            |_, _| true
        ));
        assert!(!filtered.is_current(5));
        assert_eq!(list.item_count(), 2);
    }
}
