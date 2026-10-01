//! Settings interaction tests use real views with offline channels and disabled
//! fixture persistence. They never load accounts or write user configuration.

use super::*;
use gpui::{Focusable, TestAppContext, VisualTestContext};

struct Harness {
    app: Entity<BackseaterApp>,
    view: Entity<SettingsView>,
    cx: VisualTestContext,
    _runtime: tokio::runtime::Runtime,
}

impl Harness {
    fn new(panel: Panel, cx: &mut TestAppContext) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let guard = runtime.enter();
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::image_cache::LruImageCache::install_for_test(cx);
        });
        let mut app = None;
        cx.add_window(|window, cx| {
            let entity = cx.new(|cx| {
                let session = Session::for_test();
                let channel =
                    crate::channel_store::register_for_test(session.clone(), "fixture", cx);
                let mention_store = cx.new(|_| MentionStore::default());
                let tabs = (0..2)
                    .map(|ix| {
                        let mut config = TabConfig::empty();
                        config.name = format!("tab-{ix}");
                        config.twitch_channel = "fixture".into();
                        BackseaterApp::make_tab(
                            &session,
                            config,
                            14.,
                            Default::default(),
                            Default::default(),
                            Default::default(),
                            ix as u64 + 1,
                            &mention_store,
                            window,
                            cx,
                        )
                    })
                    .collect();
                drop(channel);
                BackseaterApp {
                    session,
                    tabs,
                    active: 0,
                    dragging: None,
                    chip_tip: None,
                    chip_hovered: None,
                    chip_tip_hovered: false,
                    chip_tip_gen: 0,
                    settings: Settings {
                        custom_mentions: vec!["remember this allocation".into()],
                        ..Default::default()
                    },
                    persistence_enabled: false,
                    settings_window: None,
                    settings_view: None,
                    main_window: window.window_handle(),
                    popouts: Vec::new(),
                    popout_views: Vec::new(),
                    mentions_window: None,
                    _login_watch: cx.spawn(async |_, _| {}),
                    obs_running: false,
                    _obs_watch: cx.spawn(async |_, _| {}),
                    streamer_banner_dismissed: false,
                    update_ready: None,
                    update_banner_dismissed: false,
                    updated_to: None,
                    _update_watch: cx.spawn(async |_, _| {}),
                    window_title: String::new(),
                    mention_store,
                    mentions_tab_selected: false,
                    mentions_feed: mentions::FeedList::default(),
                    mentions_unread: false,
                    _mention_subs: Vec::new(),
                }
            });
            app = Some(entity.clone());
            Root::new(entity, window, cx)
        });
        let app = app.unwrap();
        cx.update(|cx| BackseaterApp::show_settings_window(app.clone(), panel, cx));
        let (handle, view) = cx.update(|cx| {
            let app = app.read(cx);
            (
                app.settings_window.unwrap().0,
                app.settings_view.as_ref().unwrap().upgrade().unwrap(),
            )
        });
        drop(guard);
        let mut harness = Self {
            app,
            view,
            cx: VisualTestContext::from_window(handle, cx),
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
}

#[gpui::test]
fn settings_editor_types_saves_and_cancels_in_its_own_window(cx: &mut TestAppContext) {
    let mut harness = Harness::new(Panel::App, cx);
    let count = harness.cx.update(|window, cx| {
        harness.view.update(cx, |view, cx| {
            view.settings_category = SettingsCategory::ModButtons;
            view.settings_inputs
                .mod_command
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            cx.notify();
        });
        harness.app.read(cx).settings.mod_buttons.len()
    });
    harness.draw();
    harness.cx.simulate_input("/timeout 1h test");
    harness.draw();
    harness.cx.update(|window, cx| {
        let view = harness.view.read(cx);
        assert_eq!(
            view.settings_inputs.mod_command.read(cx).value().as_str(),
            "/timeout 1h test"
        );
        assert!(view
            .settings_inputs
            .mod_command
            .read(cx)
            .focus_handle(cx)
            .is_focused(window));
        assert_eq!(harness.app.read(cx).settings.mod_buttons.len(), count);
        harness
            .view
            .update(cx, |view, cx| view.add_mod_button(window, cx));
    });
    harness.draw();
    harness.cx.update(|window, cx| {
        let app = harness.app.read(cx);
        assert_eq!(app.settings.mod_buttons.len(), count + 1);
        assert_eq!(app.settings.mod_buttons[count].command, "/timeout 1h test");
        harness.view.update(cx, |view, cx| {
            assert!(view.settings_inputs.mod_command.read(cx).value().is_empty());
            view.edit_mod_button(count, window, cx);
            view.settings_inputs
                .mod_command
                .update(cx, |input, cx| input.set_value("/ban", window, cx));
            view.cancel_mod_button_edit(window, cx);
        });
        assert_eq!(
            harness.app.read(cx).settings.mod_buttons[count].command,
            "/timeout 1h test"
        );
    });
}

#[gpui::test]
fn settings_tab_save_tracks_identity_through_reorder(cx: &mut TestAppContext) {
    let mut harness = Harness::new(Panel::Tab(0), cx);
    harness.cx.update(|window, cx| {
        harness.view.update(cx, |view, cx| {
            view.settings_inputs
                .name
                .update(cx, |input, cx| input.set_value("renamed", window, cx));
        });
        harness.app.update(cx, |app, cx| {
            app.move_tab(0, 1, cx);
            app.tabs[1].config.events_only = true;
        });
        // Save before the settings view consumes the reorder notification.
        harness
            .view
            .update(cx, |view, cx| view.apply_settings(0, cx));
        let app = harness.app.read(cx);
        assert_eq!(app.tabs[0].id, 2);
        assert_eq!(app.tabs[0].config.name, "tab-1");
        assert_eq!(app.tabs[1].id, 1);
        assert_eq!(app.tabs[1].config.name, "renamed");
        assert!(app.tabs[1].config.events_only);
    });
    harness.draw();
    harness.cx.update(|_, cx| {
        assert!(matches!(harness.view.read(cx).panel, Panel::Tab(1)));
    });
}

#[gpui::test]
fn settings_snapshot_ignores_unrelated_notifications_and_theme_draft_stays_local(
    cx: &mut TestAppContext,
) {
    let mut harness = Harness::new(Panel::App, cx);
    let pointer = harness
        .cx
        .update(|_, cx| harness.view.read(cx).settings.custom_mentions[0].as_ptr());
    harness
        .cx
        .update(|_, cx| harness.app.update(cx, |_, cx| cx.notify()));
    harness.draw();
    harness.cx.update(|window, cx| {
        assert_eq!(
            harness.view.read(cx).settings.custom_mentions[0].as_ptr(),
            pointer
        );
        harness.view.update(cx, |view, cx| {
            view.new_theme(window, cx);
            view.set_theme_color(ThemeColorField::ChatBg, 0x123456, cx);
            assert_eq!(view.theme_draft.as_ref().unwrap().chat_bg, 0x123456);
            view.cancel_theme_edit(cx);
            assert!(view.theme_draft.is_none());
        });
        assert!(harness.app.read(cx).settings.custom_themes.is_empty());
        assert_eq!(
            harness.app.read(cx).settings.theme,
            settings::ThemeChoice::Dark
        );
        // A relevant change does update the open view through its event subscription.
        harness
            .app
            .update(cx, |app, cx| app.set_mention_sound(false, cx));
    });
    harness.draw();
    harness
        .cx
        .update(|_, cx| assert!(!harness.view.read(cx).settings.mention_sound));
}
