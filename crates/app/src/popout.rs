//! Popped-out chat windows.
//!
//! Popping out a tab opens a separate OS window showing that channel's feed +
//! input — a **second, independent [`ChatView`] on the same channel**. Because
//! `ChatView::new` joins-or-attaches the shared `channel_store` model by
//! `ChannelKey`, the popout reuses the tab's message buffer + connection (no
//! duplicate network). The original tab stays in the main strip untouched, so a
//! popout is a *mirror*, not a move — closing the window just drops the extra
//! view, never the channel. Many popouts of the same channel are allowed.
//!
//! Unlike a settings panel (`child_window.rs`, a padded body rendered against a
//! host), a popout hosts a full `ChatView` directly, so it gets its own root
//! view rendered edge-to-edge rather than reusing `ChildWindow`'s panel chrome.

use gpui::prelude::*;
use gpui::{
    px, AnyWindowHandle, App, Bounds, DisplayId, Entity, Pixels, SharedString, Size, Subscription,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions,
};
use gpui_component::{ActiveTheme, Root};

use crate::chatview::ChatView;
use crate::mentions::MentionStore;
use crate::session::Session;
use crate::tabs::TabConfig;

/// Default size of a popped-out chat window. Roomy enough for a readable feed;
/// the OS resizes it freely from there.
pub const POPOUT_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(420.),
    height: px(640.),
};
/// Smallest a popout can be resized to — enough for the input bar + a few rows.
pub const POPOUT_MIN_SIZE: Size<Pixels> = Size {
    width: px(300.),
    height: px(240.),
};

/// The root content of a popout window: it owns the popped-out [`ChatView`] and
/// renders it full-bleed. When the view is dropped (its shared channel torn
/// down, or the app shutting down) the window has nothing to show and removes
/// itself.
pub struct PopoutWindow {
    view: Entity<ChatView>,
}

impl PopoutWindow {
    /// The popped-out view, so the app can track it for filter refreshes
    /// (`refresh_popout_filters`).
    pub fn view(&self) -> &Entity<ChatView> {
        &self.view
    }
}

impl Render for PopoutWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Un-stick hover state if the pointer left the window (see stale_hover.rs).
        crate::stale_hover::clear(window, cx);
        // The view must render the dialog layer itself for `open_alert_dialog`
        // (pin/unpin confirmations) to appear in this window.
        let dialog_layer = Root::render_dialog_layer(window, cx);
        gpui::div()
            .size_full()
            .bg(cx.theme().background)
            .child(self.view.clone())
            .children(dialog_layer)
    }
}

/// All the app state a popout's `ChatView` needs, gathered on the main window
/// side (where these are cheap to read) and handed to [`open`], which builds the
/// view against the popout window (kit inputs bind to the window they're created
/// in, so the `ChatView` must be constructed inside the new window).
pub struct PopoutParams {
    pub session: Session,
    pub config: TabConfig,
    pub font_size: f32,
    pub mentions: bks_core::MentionMatcher,
    pub ignore: bks_core::IgnoreList,
    pub suppress: bks_core::SuppressList,
    pub tab_id: u64,
    pub mention_store: Entity<MentionStore>,
}

/// Opens a popout window for the given tab. `parent_display` is the display the
/// main window is on — it must travel with the bounds (see `child_window::open`
/// for why gpui otherwise relocates the window). Returns the window handle so
/// the app can close it on shutdown; observe the `PopoutWindow` release (via the
/// returned entity path in the caller) to learn the user closed it.
pub fn open(
    params: PopoutParams,
    bounds: Bounds<Pixels>,
    parent_display: Option<DisplayId>,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<PopoutWindow>)> {
    let display_id = crate::child_window::resolve_display(bounds, parent_display, cx);

    let title = if params.config.name.is_empty() {
        "Backseater".to_string()
    } else {
        format!("Backseater - {}", params.config.name)
    };

    let mut content = None;
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id,
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(title)),
                ..Default::default()
            }),
            window_min_size: Some(POPOUT_MIN_SIZE),
            ..Default::default()
        },
        |window, cx| {
            // Build the ChatView against THIS window so its InputStates bind here.
            let view = cx.new(|cx| {
                ChatView::new(
                    params.session,
                    params.config,
                    params.font_size,
                    params.mentions,
                    params.ignore,
                    params.suppress,
                    params.tab_id,
                    params.mention_store,
                    window,
                    cx,
                )
            });
            // Focus the composer so typing / Ctrl+R work without a click first.
            view.update(cx, |v, cx| v.focus_composer(window, cx));
            let popout = cx.new(|_| PopoutWindow { view });
            content = Some(popout.clone());
            // The kit's Root supplies this window's tooltip/popover/dialog layers
            // (the usercard opened from a popout, emote popups, etc.).
            cx.new(|cx| Root::new(popout, window, cx))
        },
    )?;
    Ok((handle.into(), content.expect("build_root_view always runs")))
}

/// The root content of the popped-out global Mentions window. Unlike a channel
/// popout (which owns a `ChatView`), the Mentions feed has no channel — its body
/// renders against `BackseaterApp` (it reads the shared `mention_store` +
/// scroll/new-flag state), so this holds a weak handle to the app and re-renders
/// whenever it notifies (a new mention arrives). Rendered full-bleed; the feed
/// manages its own scroll. Not built on `child_window.rs` because that wraps the
/// body in a padded, separately-scrolling panel that would fight the feed.
pub struct MentionsWindow {
    app: WeakEntity<crate::BackseaterApp>,
    _observe_app: Subscription,
}

impl Render for MentionsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Un-stick hover state if the pointer left the window (see stale_hover.rs).
        crate::stale_hover::clear(window, cx);
        let Some(app) = self.app.upgrade() else {
            // The app is gone (shutting down): nothing to show.
            window.remove_window();
            return gpui::div().into_any_element();
        };
        let body = app.update(cx, |app, cx| app.mentions_tab_body(cx));
        gpui::div()
            .size_full()
            .bg(cx.theme().background)
            .child(body)
            .into_any_element()
    }
}

/// Default size of the popped-out Mentions window.
pub const MENTIONS_WINDOW_SIZE: Size<Pixels> = Size {
    width: px(420.),
    height: px(560.),
};

/// Opens the global Mentions feed in its own OS window (against `app`). Same
/// display-id rule as [`open`]. Returns the handle + content entity (observe its
/// release to learn the window closed).
pub fn open_mentions(
    app: Entity<crate::BackseaterApp>,
    bounds: Bounds<Pixels>,
    parent_display: Option<DisplayId>,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<MentionsWindow>)> {
    let display_id = crate::child_window::resolve_display(bounds, parent_display, cx);
    let title = app.read(cx).mentions_window_title();

    let mut content = None;
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id,
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(title)),
                ..Default::default()
            }),
            window_min_size: Some(POPOUT_MIN_SIZE),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| MentionsWindow {
                app: app.downgrade(),
                _observe_app: cx.observe(&app, |_, _, cx| cx.notify()),
            });
            content = Some(view.clone());
            cx.new(|cx| Root::new(view, window, cx))
        },
    )?;
    Ok((handle.into(), content.expect("build_root_view always runs")))
}
