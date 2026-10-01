use super::*;

pub(super) struct ViewerListView {
    host: gpui::WeakEntity<ChatView>,
    controller: Controller,
    data: viewerlist::ViewerList,
    pub(super) input: Entity<InputState>,
    pub(super) matches: Vec<usize>,
    pub(super) list: ListState,
    _input_sub: gpui::Subscription,
    fetch: Option<gpui::Task<()>>,
    #[cfg(test)]
    pub refilters: usize,
}

impl ViewerListView {
    pub fn new(host: &Entity<ChatView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search viewers…"));
        let sub = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::Change = event {
                this.refilter(cx);
                cx.notify();
            }
        });
        Self {
            host: host.downgrade(),
            controller: host.read(cx).controller.clone(),
            data: viewerlist::ViewerList::new(host.read(cx).config.twitch_channel.clone()),
            input,
            matches: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(40.)),
            _input_sub: sub,
            fetch: None,
            #[cfg(test)]
            refilters: 0,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.data.state = viewerlist::State::Loading;
        self.refilter(cx);
        let (tx, rx) = smol::channel::bounded(1);
        self.controller.fetch_twitch_chatters(tx);
        self.fetch = Some(cx.spawn(async move |weak, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = weak.update(cx, |this, cx| this.resolve(result, cx));
            }
        }));
        cx.notify();
    }

    pub(super) fn reconnect(
        &mut self,
        channel: &str,
        controller: Controller,
        cx: &mut Context<Self>,
    ) {
        self.controller = controller;
        if self.data.channel != channel {
            self.data.channel = channel.to_owned();
            self.refresh(cx);
        }
    }

    pub(super) fn resolve(
        &mut self,
        result: anyhow::Result<bks_twitch::Chatters>,
        cx: &mut Context<Self>,
    ) {
        self.data.resolve(result);
        self.refilter(cx);
        cx.notify();
    }

    fn refilter(&mut self, cx: &App) {
        self.matches = match &self.data.state {
            viewerlist::State::Loaded(chatters) => {
                viewerlist::filter(&chatters.chatters, &self.input.read(cx).value())
            }
            _ => Vec::new(),
        };
        self.list.reset(self.matches.len());
        #[cfg(test)]
        {
            self.refilters += 1;
        }
    }
}

impl Render for ViewerListView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::stale_hover::clear(window, cx);
        let count = match &self.data.state {
            viewerlist::State::Loaded(chatters) => {
                format!(
                    "{} {}",
                    chatters.total,
                    bks_core::plural(chatters.total, "chatter", "chatters")
                )
            }
            _ => self.data.channel.clone(),
        };
        let entity = cx.entity();
        let content = match &self.data.state {
            viewerlist::State::Loading => div().child("loading viewer list…").into_any_element(),
            viewerlist::State::Failed(error) => div()
                .text_color(gpui::rgb(0xe05d5d))
                .child(error.clone())
                .into_any_element(),
            viewerlist::State::Loaded(_) => gpui::list(self.list.clone(), move |ix, _, cx| {
                let this = entity.read(cx);
                let viewerlist::State::Loaded(chatters) = &this.data.state else {
                    return div().into_any_element();
                };
                let Some(chatter) = this
                    .matches
                    .get(ix)
                    .and_then(|&ix| chatters.chatters.get(ix))
                else {
                    return div().into_any_element();
                };
                let login = chatter.user_login.clone();
                let name = if chatter.user_name.is_empty() {
                    login.clone()
                } else {
                    chatter.user_name.clone()
                };
                let user_id = chatter.user_id.clone();
                let host = this.host.clone();
                div()
                    .id(("viewer", ix))
                    .debug_selector(move || format!("viewer-row-{ix}"))
                    .px_1()
                    .py_0p5()
                    .rounded_sm()
                    .text_size(px(13.))
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().secondary))
                    .child(viewerlist::label(chatter))
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        let _ = host.update(cx, |host, cx| {
                            host.show_usercard(
                                usercard::UserCard::new(
                                    login.clone(),
                                    name.clone(),
                                    user_id.clone(),
                                    bks_core::Platform::Twitch,
                                    None,
                                ),
                                cx,
                            )
                        });
                    })
                    .into_any_element()
            })
            .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
            .size_full()
            .into_any_element(),
        };
        v_flex()
            .size_full()
            .min_h_0()
            .p_4()
            .gap_2()
            .bg(gpui::rgb(render::panel_bg()))
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(count)
                    .child(
                        Button::new("viewerlist-refresh")
                            .label("Refresh")
                            .outline()
                            .xsmall()
                            .compact()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(Input::new(&self.input))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(content)
                    .vertical_scrollbar(&self.list),
            )
    }
}

impl ChatView {
    pub(super) fn open_viewer_list(&mut self, cx: &mut Context<Self>) {
        if self.config.twitch_channel.is_empty() {
            return;
        }
        let view = cx.entity();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| Self::show_viewer_list_window(view, cx));
        })
        .detach();
    }

    fn show_viewer_list_window(view: Entity<Self>, cx: &mut App) {
        let title = format!("Viewer List - {}", view.read(cx).config.twitch_channel);
        if let Some(handle) = view.read(cx).viewer_list_window {
            if child_window::focus_existing(handle, Some(&title), cx) {
                if let Some(content) = view
                    .read(cx)
                    .viewer_list_view
                    .as_ref()
                    .and_then(|v| v.upgrade())
                {
                    content.update(cx, |this, cx| this.refresh(cx));
                }
                return;
            }
        }
        let host = view.clone();
        let Ok((handle, content)) = child_window::open_owned(
            &title,
            VIEWERLIST_WINDOW_SIZE,
            VIEWERLIST_MIN_SIZE,
            view.read(cx).parent_window,
            None,
            move |window, cx| ViewerListView::new(&host, window, cx),
            cx,
        ) else {
            return;
        };
        content.update(cx, |this, cx| this.refresh(cx));
        view.update(cx, |this, cx| {
            this.viewer_list_window = Some(handle);
            this.viewer_list_view = Some(content.downgrade());
            cx.observe_release(&content, move |this, _, _| {
                if this.viewer_list_window == Some(handle) {
                    this.viewer_list_window = None;
                    this.viewer_list_view = None;
                }
            })
            .detach();
        });
    }
}
