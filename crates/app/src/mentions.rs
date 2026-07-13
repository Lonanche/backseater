//! The app-wide mention feed: every tab pushes its mention-matched live
//! messages here, so a mentions panel in "all tabs" mode (per-tab setting) and
//! the global Mentions tab (app setting) can show mentions from every tab.
//! Each row carries a "#channel" tag; clicking a row emits [`ActivateTab`],
//! which the app maps back to the source tab and selects it.

use std::collections::VecDeque;

use bks_core::Message;
use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, EventEmitter, SharedString, WeakEntity};
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
    entries: VecDeque<MentionEntry>,
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
                .any(|e| e.msg.platform == entry.msg.platform && e.msg.id == entry.msg.id)
        {
            return;
        }
        // The store is the one point a live mention passes exactly once
        // app-wide (deduped above), so the alert ping plays here: master
        // toggle on, matched term unmuted, and not silenced by streamer mode.
        if entry.sound
            && crate::settings::mention_sound_enabled()
            && !(crate::streamer_mode::is_active() && crate::settings::streamer_mute_sounds())
        {
            crate::sound::play_mention_ping();
        }
        self.entries.push_back(entry);
        if self.entries.len() > MAX_MENTIONS {
            self.entries.pop_front();
        }
        cx.notify();
    }

    /// Drops a closed tab's mentions so the feed doesn't offer dead jumps.
    pub fn remove_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let before = self.entries.len();
        self.entries.retain(|e| e.tab_id != tab_id);
        if self.entries.len() != before {
            cx.notify();
        }
    }
}

/// Renders the shared feed as chat-style rows, each under a small "#channel"
/// tag. Clicking a row activates its source tab; clicking the author opens the
/// usercard on that tab's view (propagation stopped so it doesn't also jump).
/// Shared by the per-tab mentions panel in "all tabs" mode and the global
/// Mentions tab.
pub fn feed_rows(store: &Entity<MentionStore>, font_size: f32, cx: &App) -> Vec<gpui::AnyElement> {
    // Not part of any log's drag-select; a throwaway selection context (same
    // as the usercard's message list).
    let selection = selectable::Selection::new();
    selection.begin_frame();
    let mut ordinal = 0usize;
    store
        .read(cx)
        .entries
        .iter()
        .enumerate()
        .map(|(ix, entry)| {
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
        })
        .collect()
}
