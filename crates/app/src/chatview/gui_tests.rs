//! Headless interaction tests for the production chat view, using GPUI's test
//! platform. No saved accounts, settings, live connections, or disk image cache.

use super::*;
use bks_core::{Emote, EmoteTooltip, Platform};
use bks_platform::ChatEvent;
use gpui::{Focusable, KeyDownEvent, Keystroke, Modifiers, TestAppContext, VisualTestContext};
use gpui_component::Root;

struct Harness {
    view: Entity<ChatView>,
    cx: VisualTestContext,
    // Controller tasks stay parked: this current-thread runtime is never driven.
    // Input events and rendering run on GPUI's deterministic test executor.
    _runtime: tokio::runtime::Runtime,
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let guard = runtime.enter();
        cx.update(|cx| {
            gpui_component::init(cx);
            LruImageCache::install_for_test(cx);
        });
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let session = Session::for_test();
            let channel = crate::channel_store::register_for_test(session.clone(), "fixture", cx);
            let mention_store = cx.new(|_| crate::mentions::MentionStore::default());
            let mut config = TabConfig::empty();
            config.twitch_channel = "fixture".into();
            let chat = cx.new(|cx| {
                ChatView::new(
                    session,
                    config,
                    14.,
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    1,
                    mention_store,
                    window,
                    cx,
                )
            });
            channel.update(cx, |channel, cx| {
                channel.push(
                    ChatEvent::Emotes {
                        platform: Platform::Twitch,
                        emotes: vec![emote("Kappa"), emote("KappaPride")],
                    },
                    cx,
                );
            });
            chat.read(cx)
                .input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            view = Some(chat.clone());
            Root::new(chat, window, cx)
        });
        drop(guard);
        let mut harness = Self {
            view: view.unwrap(),
            cx: VisualTestContext::from_window(window.into(), cx),
            _runtime: runtime,
        };
        harness.draw();
        harness
    }

    fn draw(&mut self) {
        self.cx.run_until_parked();
        self.cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
    }

    fn type_text(&mut self, text: &str) {
        self.cx.simulate_input(text);
        self.draw();
    }

    fn keys(&mut self, keys: &str) {
        for key in keys.split_whitespace() {
            // Dispatch physical keys without GPUI's simulated IME fallback,
            // which inserts a newline after a propagating single-line Enter.
            self.cx.simulate_event(KeyDownEvent {
                keystroke: Keystroke::parse(key).unwrap(),
                is_held: false,
                prefer_character_input: false,
            });
            self.draw();
        }
    }

    fn click(&mut self, selector: &'static str) {
        self.draw();
        let bounds = self
            .cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing rendered control: {selector}"));
        assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
        self.cx
            .simulate_click(bounds.center(), Modifiers::default());
        self.draw();
    }

    fn text(&mut self) -> String {
        self.cx
            .update(|_, cx| self.view.read(cx).input.read(cx).value().to_string())
    }

    fn assert_composer_focused(&mut self) {
        self.cx.update(|window, cx| {
            assert!(self
                .view
                .read(cx)
                .input
                .read(cx)
                .focus_handle(cx)
                .is_focused(window));
        });
    }

    fn assert_not_submitted(&mut self) {
        self.cx
            .update(|_, cx| assert!(self.view.read(cx).sent_history.is_empty()));
    }
}

fn emote(name: &str) -> Emote {
    Emote {
        id: name.into(),
        name: name.into(),
        // The offline cache leaves images blank; these tests exercise input.
        url: format!("test-emotes/{name}.png"),
        animated: false,
        tooltip: EmoteTooltip::provider("7TV"),
    }
}

#[gpui::test]
fn gui_history_restores_the_draft_after_browsing_sent_messages(cx: &mut TestAppContext) {
    let mut app = Harness::new(cx);
    app.type_text("first message");
    app.keys("enter");
    assert_eq!(app.text(), "");
    app.type_text("second message");
    app.keys("enter");
    app.type_text("unfinished draft");

    app.keys("up");
    assert_eq!(app.text(), "second message");
    app.keys("up");
    assert_eq!(app.text(), "first message");
    app.keys("down");
    assert_eq!(app.text(), "second message");
    app.keys("down");
    assert_eq!(app.text(), "unfinished draft");
    app.assert_composer_focused();
}

#[gpui::test]
fn gui_tab_cycles_emotes_without_moving_focus(cx: &mut TestAppContext) {
    let mut app = Harness::new(cx);
    app.type_text("Kap");
    app.keys("tab");
    assert_eq!(app.text(), "Kappa ");
    app.assert_composer_focused();
    app.keys("tab");
    assert_eq!(app.text(), "KappaPride ");
    app.assert_composer_focused();
    app.assert_not_submitted();
}

#[gpui::test]
fn gui_popup_arrows_and_enter_complete_without_sending(cx: &mut TestAppContext) {
    let mut app = Harness::new(cx);
    app.type_text(":Kap");
    app.cx.update(|_, cx| {
        assert_eq!(app.view.read(cx).popup.as_ref().unwrap().items.len(), 2);
    });
    app.keys("down enter");
    assert_eq!(app.text(), "KappaPride ");
    app.assert_not_submitted();
    app.assert_composer_focused();
}

#[gpui::test]
fn gui_escape_dismisses_completion_and_preserves_typed_text(cx: &mut TestAppContext) {
    let mut app = Harness::new(cx);
    app.type_text(":Kap");
    app.cx
        .update(|_, cx| assert!(app.view.read(cx).popup.is_some()));
    app.keys("escape");
    app.cx
        .update(|_, cx| assert!(app.view.read(cx).popup.is_none()));
    assert_eq!(app.text(), ":Kap");
    app.assert_not_submitted();
    app.assert_composer_focused();
}

#[gpui::test]
fn gui_picker_filters_inserts_and_closes_with_mouse_input(cx: &mut TestAppContext) {
    let mut app = Harness::new(cx);
    app.type_text("hello");
    app.click("emote-picker-toggle");
    assert!(app.cx.debug_bounds("emote-picker").is_some());
    app.click("picker-search");
    app.type_text("pride");
    app.cx.update(|_, cx| {
        let chat = app.view.read(cx);
        let names: Vec<_> = chat
            .picker_rows
            .iter()
            .filter_map(|row| match row {
                PickerRow::Emotes(emotes) => Some(emotes),
                _ => None,
            })
            .flatten()
            .map(|emote| emote.name.as_str())
            .collect();
        assert_eq!(names, ["KappaPride"]);
    });
    assert!(app.cx.debug_bounds("picker-emote-Kappa").is_none());
    app.click("picker-emote-KappaPride");
    assert_eq!(app.text(), "hello KappaPride ");
    app.assert_not_submitted();
    app.click("emote-picker-toggle");
    assert!(app.cx.debug_bounds("emote-picker").is_none());
}
