//! The app-wide mention feed: every tab pushes its mention-matched live
//! messages here, so a mentions panel in "all tabs" mode (per-tab setting) and
//! the global Mentions tab (app setting) can show mentions from every tab.
//! Each row carries a "#channel" tag; clicking a row emits [`ActivateTab`],
//! which the app maps back to the source tab and selects it.

use std::collections::VecDeque;

use bks_core::Message;
use gpui::prelude::*;
use gpui::{
    div, list, px, App, Context, Entity, EventEmitter, FollowMode, ListAlignment, ListState,
    SharedString, WeakEntity,
};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{v_flex, ActiveTheme};

use crate::chatview::ChatView;
use crate::{render, selectable};

/// How many mentions the shared feed keeps; the oldest drop past this.
const MAX_MENTIONS: usize = 300;

/// One recorded mention. `source` is the channel the message arrived on (its
/// platform's channel name), `tab_id` the owning tab (clicking the row
/// activates it), and `view` that tab's live view — clicking the author opens
/// the usercard there. The weak view dangles harmlessly after a channel-swap
/// rebuild (the name click just no-ops); the id survives it.
pub struct MentionEntry {
    pub tab_id: u64,
    pub source: SharedString,
    pub view: WeakEntity<ChatView>,
    pub msg: Box<Message>,
    /// Whether the matched term(s) want the alert ping (per-term mute already
    /// applied by the matcher); the master/streamer-mode gates apply at play.
    pub sound: bool,
}

/// The shared mention list, an entity so consumers re-render when one arrives
/// (`observe`) and row clicks flow back to the app as events (`subscribe`),
/// without tabs needing a handle to the app.
#[derive(Default)]
pub struct MentionStore {
    entries: VecDeque<(u64, MentionEntry)>,
    next_seq: u64,
    generation: u64,
}

/// Emitted when a mention row is clicked: the app should select the tab with
/// `tab_id` (a no-op if it was closed since), then jump its view to the
/// mentioned message (`platform` + `msg_id`), flashing it — or, if it has aged
/// out of that tab's buffer, showing a transient "no longer in history" note.
pub struct ActivateTab {
    pub tab_id: u64,
    pub platform: bks_core::Platform,
    pub msg_id: String,
}

impl EventEmitter<ActivateTab> for MentionStore {}

impl MentionStore {
    pub fn push(&mut self, entry: MentionEntry, cx: &mut Context<Self>) {
        // De-duplicate by the message's identity: when the same channel is open in
        // several tabs they share one buffer, so each of those views matches and
        // pushes the *same* mention — record it once. (A blank id, e.g. a synthetic
        // row, can't be deduped, so it always passes.)
        if !entry.msg.id.is_empty()
            && self
                .entries
                .iter()
                .any(|(_, e)| e.msg.platform == entry.msg.platform && e.msg.id == entry.msg.id)
        {
            return;
        }
        // The store is the one point a live mention passes exactly once
        // app-wide (deduped above), so the alert ping plays here: master
        // toggle on and matched term unmuted (`play_ping` itself applies the
        // streamer-mode mute).
        if entry.sound && crate::settings::mention_sound_enabled() {
            crate::sound::play_ping();
        }
        self.entries.push_back((self.next_seq, entry));
        self.next_seq += 1;
        self.generation += 1;
        if self.entries.len() > MAX_MENTIONS {
            self.entries.pop_front();
        }
        cx.notify();
    }

    /// Drops a closed tab's mentions so the feed doesn't offer dead jumps.
    pub fn remove_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let before = self.entries.len();
        self.entries.retain(|(_, e)| e.tab_id != tab_id);
        if self.entries.len() != before {
            self.generation += 1;
            cx.notify();
        }
    }
}

/// Shared tail-following behavior for local and app-wide mention feeds.
pub fn list_state() -> ListState {
    let state = ListState::new(0, ListAlignment::Bottom, px(80.));
    state.set_follow_mode(FollowMode::Tail);
    state
}

/// Preserve scroll anchors for ordinary front trims and tail appends.
pub fn sync_list<T: PartialEq>(state: &ListState, old: &[T], new: &[T]) {
    if old == new {
        return;
    }
    let drop_front = new
        .first()
        .and_then(|first| old.iter().position(|v| v == first))
        .unwrap_or(old.len());
    let kept = old.len() - drop_front;
    if kept <= new.len() && old[drop_front..] == new[..kept] {
        if drop_front > 0 {
            state.splice(0..drop_front, 0);
        }
        if new.len() > kept {
            state.splice(kept..kept, new.len() - kept);
        }
    } else {
        state.reset(new.len());
    }
}

pub fn list_body(
    id: &'static str,
    state: &ListState,
    font_size: f32,
    content: impl IntoElement,
) -> gpui::AnyElement {
    div()
        .id(id)
        .relative()
        .flex_1()
        .min_h_0()
        .text_size(px(font_size))
        .child(content)
        .when(!state.is_following_tail(), |d| d.vertical_scrollbar(state))
        .into_any_element()
}

pub struct FeedList {
    state: ListState,
    keys: Vec<u64>,
    generation: Option<u64>,
    font_size: f32,
}

impl Default for FeedList {
    fn default() -> Self {
        Self {
            state: list_state(),
            keys: Vec::new(),
            generation: None,
            font_size: 0.,
        }
    }
}

impl FeedList {
    pub fn remeasure(&self) {
        self.state.remeasure();
    }

    pub fn render(
        &mut self,
        store: &Entity<MentionStore>,
        font_size: f32,
        cx: &App,
    ) -> gpui::AnyElement {
        let model = store.read(cx);
        if self.generation != Some(model.generation) {
            let keys = model
                .entries
                .iter()
                .map(|(seq, _)| *seq)
                .collect::<Vec<_>>();
            sync_list(&self.state, &self.keys, &keys);
            self.keys = keys;
            self.generation = Some(model.generation);
        }
        if self.font_size != font_size {
            self.font_size = font_size;
            self.state.remeasure();
        }
        if self.keys.is_empty() {
            return div()
                .px_3()
                .py_2()
                .text_size(px(font_size * 0.85))
                .text_color(cx.theme().muted_foreground)
                .child("No mentions yet.")
                .into_any_element();
        }
        let store = store.clone();
        let content = list(self.state.clone(), move |ix, _, cx| {
            let Some((_, entry)) = store.read(cx).entries.get(ix) else {
                return div().into_any_element();
            };
            div()
                .px(px(6.))
                .pb_1()
                .child(feed_row(entry, &store, ix, font_size, cx))
                .into_any_element()
        })
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .size_full();
        list_body("mentions-feed", &self.state, font_size, content)
    }
}

fn feed_row(
    entry: &MentionEntry,
    store: &Entity<MentionStore>,
    ix: usize,
    font_size: f32,
    cx: &App,
) -> gpui::AnyElement {
    let selection = selectable::Selection::new();
    let mut ordinal = 0;
    let name_click: render::NameClick = {
        let view = entry.view.clone();
        let msg_id = SharedString::from(entry.msg.id.clone());
        Box::new(move |_window, cx| {
            cx.stop_propagation();
            let _ = view.update(cx, |this, cx| {
                this.open_usercard(&msg_id, cx);
                cx.notify();
            });
        })
    };
    let mention_click: render::MentionClick = {
        let view = entry.view.clone();
        let platform = entry.msg.platform;
        std::rc::Rc::new(move |login: &str, _window, cx| {
            cx.stop_propagation();
            let _ = view.update(cx, |this, cx| {
                this.open_usercard_named(login, platform, cx);
                cx.notify();
            });
        })
    };
    let row = render::render_message(
        &entry.msg,
        render::RowFlags {
            struck: false,
            mentioned: true,
            hide_timestamp: !crate::settings::show_timestamps_mentions(),
            ..Default::default()
        },
        font_size,
        &selection,
        &mut ordinal,
        render::RowHandlers {
            name_click: Some(name_click),
            mention_click: Some(mention_click),
            ..Default::default()
        },
    );
    let tab_id = entry.tab_id;
    let platform = entry.msg.platform;
    let msg_id = entry.msg.id.clone();
    let store = store.clone();
    v_flex()
        .id(("mention-row", ix))
        .debug_selector({
            let id = entry.msg.id.clone();
            move || format!("shared-mention-{id}")
        })
        .cursor_pointer()
        .child(
            div()
                .text_size(px(font_size * 0.72))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(format!("#{}", entry.source))),
        )
        .child(row)
        .on_click(move |_, _, cx| {
            let msg_id = msg_id.clone();
            store.update(cx, |_, cx| {
                cx.emit(ActivateTab {
                    tab_id,
                    platform,
                    msg_id,
                })
            });
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_trim_and_append_preserve_the_reading_anchor() {
        let state = list_state();
        sync_list(&state, &[], &[1, 2, 3, 4]);
        state.scroll_to(gpui::ListOffset {
            item_ix: 2,
            offset_in_item: px(7.),
        });
        sync_list(&state, &[1, 2, 3, 4], &[2, 3, 4, 5, 6]);
        assert_eq!(state.item_count(), 5);
        assert_eq!(state.logical_scroll_top().item_ix, 1);
        assert_eq!(state.logical_scroll_top().offset_in_item, px(7.));
        assert!(!state.is_following_tail());
    }
}
