use super::*;
use crate::channel_store::{ChannelEvent, ChannelModel};

pub(super) struct SearchView {
    host: gpui::WeakEntity<ChatView>,
    channel: Entity<ChannelModel>,
    input: Entity<InputState>,
    query: String,
    list: ListState,
    pub(super) results: filtered::FilteredMessages,
    font_size: f32,
    mentions: bks_core::MentionMatcher,
    ignore: bks_core::IgnoreList,
    suppress: bks_core::SuppressList,
    image_cache: Entity<LruImageCache>,
    _input_sub: gpui::Subscription,
    _channel_sub: gpui::Subscription,
}

impl SearchView {
    pub(super) fn new(
        host: &Entity<ChatView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search messages…"));
        input.update(cx, |state, cx| state.focus(window, cx));
        let input_sub = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
            if let InputEvent::Change = event {
                this.query = search::normalize(&input.read(cx).value());
                this.rebuild(cx);
                this.list.set_follow_mode(FollowMode::Tail);
                cx.notify();
            }
        });
        let owner = host.read(cx);
        let channel = owner.channel.clone();
        let mut this = Self {
            host: host.downgrade(),
            channel: channel.clone(),
            input,
            query: String::new(),
            list: fresh_search_list_state(),
            results: filtered::FilteredMessages::default(),
            font_size: owner.font_size,
            mentions: owner.mentions.clone(),
            ignore: owner.ignore.clone(),
            suppress: owner.suppress.clone(),
            image_cache: owner.image_cache.clone(),
            _input_sub: input_sub,
            _channel_sub: cx.subscribe(&channel, Self::on_channel_event),
        };
        this.rebuild(cx);
        this
    }

    fn rebuild(&mut self, cx: &App) {
        let model = self.channel.read(cx);
        self.results.rebuild(
            &model.rows,
            model.rows_generation(),
            &self.list,
            |plain, msg| {
                plain && search::matches(msg, &self.query) && !self.ignore.matches_message(msg)
            },
        );
    }

    fn on_channel_event(
        &mut self,
        _: Entity<ChannelModel>,
        event: &ChannelEvent,
        cx: &mut Context<Self>,
    ) {
        let changed = self.results.apply(event, &self.list, |plain, msg| {
            plain && search::matches(msg, &self.query) && !self.ignore.matches_message(msg)
        });
        if matches!(event, ChannelEvent::RowsChanged(_)) {
            self.list.remeasure();
            cx.notify();
        } else if changed || self.results.needs_rebuild() || matches!(event, ChannelEvent::Repaint)
        {
            cx.notify();
        }
    }

    fn update_options(&mut self, owner: &ChatView, refilter: bool, cx: &mut Context<Self>) {
        self.font_size = owner.font_size;
        self.mentions = owner.mentions.clone();
        self.ignore = owner.ignore.clone();
        self.suppress = owner.suppress.clone();
        let channel_changed = self.channel != owner.channel;
        if channel_changed {
            self.channel = owner.channel.clone();
            self._channel_sub = cx.subscribe(&self.channel, Self::on_channel_event);
        }
        if channel_changed || refilter {
            self.rebuild(cx);
        } else {
            self.list.remeasure();
        }
        cx.notify();
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::stale_hover::clear(window, cx);
        if !self
            .results
            .is_current(self.channel.read(cx).rows_generation())
        {
            self.rebuild(cx);
        }
        let body = self.search_body(cx);
        div()
            .size_full()
            .p_4()
            .bg(gpui::rgb(render::panel_bg()))
            .text_color(cx.theme().foreground)
            .child(body)
    }
}

impl ChatView {
    pub(crate) fn open_search(&mut self, cx: &mut Context<Self>) {
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| Self::show_search_window(view, cx));
        })
        .detach();
    }

    fn show_search_window(view: Entity<Self>, cx: &mut App) {
        let title = format!("Search - {}", view.read(cx).config.display_name());
        if let Some(handle) = view.read(cx).search_window {
            if child_window::focus_existing(handle, Some(&title), cx) {
                if let Some(search) = view.read(cx).search_view.as_ref().and_then(|v| v.upgrade()) {
                    let _ = handle.update(cx, |_, window, cx| {
                        search
                            .read(cx)
                            .input
                            .clone()
                            .update(cx, |input, cx| input.focus(window, cx));
                    });
                }
                return;
            }
        }
        let host = view.clone();
        let Ok((handle, content)) = child_window::open_owned(
            &title,
            SEARCH_WINDOW_SIZE,
            SEARCH_MIN_SIZE,
            view.read(cx).parent_window,
            None,
            move |window, cx| SearchView::new(&host, window, cx),
            cx,
        ) else {
            return;
        };
        view.update(cx, |this, cx| {
            this.search_window = Some(handle);
            this.search_view = Some(content.downgrade());
            cx.observe_release(&content, move |this, _, _| {
                if this.search_window == Some(handle) {
                    this.search_window = None;
                    this.search_view = None;
                }
            })
            .detach();
        });
    }

    pub(super) fn update_search_options(&self, refilter: bool, cx: &mut Context<Self>) {
        if let Some(handle) = self.search_window {
            let title = format!("Search - {}", self.config.display_name());
            let _ = handle.update(cx, |_, window, _| window.set_window_title(&title));
        }
        if let Some(view) = self.search_view.as_ref().and_then(|v| v.upgrade()) {
            view.update(cx, |view, cx| view.update_options(self, refilter, cx));
        }
    }
}

impl SearchView {
    fn search_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let total = self.results.rows.len();
        let unit = bks_core::plural(total as u64, "message", "messages");
        let header_text = if self.query.is_empty() {
            // Just the shown count — "in history" would overstate it: rows the
            // view ignores are (correctly) excluded, like in the log.
            format!("{total} {unit}")
        } else {
            format!("{total} matching {unit}")
        };
        let header = div()
            .text_size(px(13.))
            .text_color(cx.theme().muted_foreground)
            .child(SharedString::from(header_text));

        let results: gpui::AnyElement = if total == 0 {
            div()
                .flex_1()
                .min_h_0()
                .pt_2()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("No matching messages"))
                .into_any_element()
        } else {
            let font_size = self.font_size;
            let view = cx.entity();
            // One throwaway selection context shared by all rows (they aren't
            // part of any drag-select), built once per body render — not per
            // visible row per frame (it's an Rc allocation).
            let selection = selectable::Selection::new();
            selection.begin_frame();
            let search_list =
                gpui::list(self.list.clone(), move |ix, _window, cx: &mut gpui::App| {
                    let this = view.read(cx);
                    let model = this.channel.read(cx);
                    let Some(msg) = this.results.rows.get(ix) else {
                        return div().into_any_element();
                    };
                    // Struck (ban/delete) + cosmetics resolve against the live
                    // model per build, same as the log's rows.
                    let struck = model.is_struck(msg);
                    let mentioned = this.mentions.matches(&msg.raw_text);
                    let suppressed = this.suppress.matches_message(msg);
                    let suspicious = model.suspicious_for(msg).map(|m| render::SuspiciousTag {
                        restricted: m.status == bks_platform::SuspiciousStatus::Restricted,
                        detail: m.detail.clone(),
                    });
                    // Ordinals only need ordering, so the row index strides
                    // them like the log.
                    let mut ordinal = ix * crate::ORDINAL_STRIDE;
                    let row = render::render_message(
                        msg,
                        render::RowFlags {
                            struck,
                            cosmetics: model.cosmetics_for(msg.platform, &msg.author.user_id),
                            mentioned,
                            hide_timestamp: !crate::settings::show_timestamps_chat(),
                            suppressed,
                            suspicious,
                            ..Default::default()
                        },
                        font_size,
                        &selection,
                        &mut ordinal,
                        render::RowHandlers::default(),
                    );
                    // An Arc bump (not an id-String clone) identifies the row
                    // for the click; capturing `ix` instead would mis-target
                    // after a splice between paint and click.
                    let msg = msg.clone();
                    let entity = this.host.clone();
                    div()
                        .id(("search-hit", ix))
                        .debug_selector(move || format!("search-hit-{ix}-struck-{struck}"))
                        .w_full()
                        .min_w_0()
                        .px(px(6.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(render::row_hover()))
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                this.jump_to_message(msg.platform, &msg.id, cx);
                                // Bring the chat window forward so the jump is
                                // seen; the app re-selects this tab
                                // (`ActivateRequested`) in case the user
                                // switched tabs meanwhile.
                                let _ = this
                                    .parent_window
                                    .update(cx, |_, window, _| window.activate_window());
                                cx.emit(ActivateRequested);
                            });
                        })
                        .child(row)
                        .into_any_element()
                })
                .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                .size_full();

            // On the chat log's (lighter) background so the rows read exactly
            // like the log; images route through the app-wide LRU cache so the
            // sweep sees them (same as the log's rows). Overlay scrollbar only
            // while scrolled off the bottom, like the log's.
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .rounded_md()
                .bg(gpui::rgb(render::chat_bg()))
                .py_1()
                .text_size(px(font_size))
                .child(
                    gpui::image_cache(self.image_cache.clone())
                        .size_full()
                        .child(search_list),
                )
                .when(!self.list.is_following_tail(), |d| {
                    d.vertical_scrollbar(&self.list)
                })
                .into_any_element()
        };

        let mut body = v_flex().h_full().gap_2().child(header);
        body = body.child(Input::new(&self.input));
        body.child(results).into_any_element()
    }
}
