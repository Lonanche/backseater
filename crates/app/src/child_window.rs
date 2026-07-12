//! Child OS windows hosting app panels (settings, usercard).
//!
//! These used to be in-app draggable overlays (`FloatWindow`); they're now real
//! OS windows (`cx.open_window`) so they can be dragged off the main window and
//! resized freely by the OS. A [`ChildWindow`] is a window's root content: it
//! holds a weak handle to a host entity (the app, or a tab's view) plus a body
//! builder that renders the panel *against the host*, so all panel state stays
//! on the host — the window is just a surface. It observes the host (re-renders
//! whenever the host notifies) and removes its own window if the host is dropped
//! (e.g. a tab rebuilt while its usercard is open), on top of the host's own
//! `on_release` cleanup.
//!
//! ⚠️ Opening a window draws it once synchronously, and this view's render
//! re-enters the host entity (`host.update`). Never call [`open`] while the host
//! is leased (i.e. from inside one of its own `update`/listener frames) — spawn
//! a task and open from a plain `App` context instead.

use gpui::prelude::*;
use gpui::{
    div, point, rgb, AnyElement, AnyWindowHandle, App, Bounds, DisplayId, Entity, Pixels,
    SharedString, Size, Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowOptions,
};
use gpui_component::{ActiveTheme, Root};

/// The parent window's screen bounds and display id, used to position a child
/// window over it (see [`open`] for why the display id must travel along). A
/// closed/invalid handle yields defaults (an empty rect on the primary display).
pub fn parent_bounds(parent: AnyWindowHandle, cx: &mut App) -> (Bounds<Pixels>, Option<DisplayId>) {
    parent
        .update(cx, |_, window, cx| {
            (window.bounds(), window.display(cx).map(|d| d.id()))
        })
        .unwrap_or_default()
}

/// The display id a child window at `bounds` should open on: hit-test the
/// bounds' center against the actual displays (so a parent straddling two
/// monitors still opens the child on the right one), falling back to
/// `parent_display`. ⚠️ This must be passed to `WindowOptions` — see [`open`]
/// for the "opens big in the wrong place" bug that omitting it causes.
pub fn resolve_display(bounds: Bounds<Pixels>, parent_display: Option<DisplayId>, cx: &App) -> Option<DisplayId> {
    cx.displays()
        .into_iter()
        .find(|d| d.bounds().contains(&bounds.center()))
        .map(|d| d.id())
        .or(parent_display)
}

/// A rect of `size` centered on `parent` — child windows always open on top of
/// the chat window; the user drags them away from there.
pub fn centered_on(parent: Bounds<Pixels>, size: Size<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: parent.center() - point(size.width / 2., size.height / 2.),
        size,
    }
}

/// Retitles (when `title` is given) and focuses an already-open child window.
/// Returns whether the handle was still alive — `false` means the user closed
/// the window under us and the caller should open a fresh one. The shared
/// "reuse" half of every open-or-focus site (usercard, viewer list, mentions).
pub fn focus_existing(handle: AnyWindowHandle, title: Option<&str>, cx: &mut App) -> bool {
    handle
        .update(cx, |_, window, _| {
            if let Some(title) = title {
                window.set_window_title(title);
            }
            window.activate_window();
        })
        .is_ok()
}

/// [`open`], centered over `parent` (the main/chat window) with the display id
/// resolved from it — the shared "open" half of every open-or-focus site.
pub fn open_centered<H: Render>(
    title: &str,
    size: Size<Pixels>,
    min_size: Size<Pixels>,
    parent: AnyWindowHandle,
    host: Entity<H>,
    body: impl Fn(&mut H, &mut gpui::Context<H>) -> AnyElement + 'static,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<ChildWindow<H>>)> {
    let (parent, display) = parent_bounds(parent, cx);
    open(
        title,
        centered_on(parent, size),
        min_size,
        display,
        host,
        body,
        cx,
    )
}

/// [`open_centered`] without the built-in padded scroll surface: the body gets
/// the window edge-to-edge and manages its own padding + scrolling. For panels
/// with their own chrome (the settings window's full-height category sidebar).
pub fn open_centered_bare<H: Render>(
    title: &str,
    size: Size<Pixels>,
    min_size: Size<Pixels>,
    parent: AnyWindowHandle,
    host: Entity<H>,
    body: impl Fn(&mut H, &mut gpui::Context<H>) -> AnyElement + 'static,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<ChildWindow<H>>)> {
    let (parent, display) = parent_bounds(parent, cx);
    open_impl(
        title,
        centered_on(parent, size),
        min_size,
        display,
        host,
        body,
        true,
        cx,
    )
}

/// The body builder a child window renders its content with, against the host.
type Body<H> = Box<dyn Fn(&mut H, &mut gpui::Context<H>) -> AnyElement>;

/// The root content view of one child window; `H` is the host entity its body
/// renders against.
pub struct ChildWindow<H: Render> {
    host: WeakEntity<H>,
    body: Body<H>,
    /// Bare windows hand the body the full window; padded ones wrap it in the
    /// shared scrolling panel surface.
    bare: bool,
    _observe_host: Subscription,
}

impl<H: Render> Render for ChildWindow<H> {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        // Un-stick hover state if the pointer left the window (see stale_hover.rs).
        crate::stale_hover::clear(window, cx);
        let Some(host) = self.host.upgrade() else {
            // The host is gone (its tab closed or was rebuilt): nothing to show.
            window.remove_window();
            return div().into_any_element();
        };
        let body = host.update(cx, |host, cx| (self.body)(host, cx));
        let root = div()
            .size_full()
            .bg(rgb(crate::render::panel_bg()))
            .text_color(cx.theme().foreground);
        if self.bare {
            return root.child(body).into_any_element();
        }
        root
            // The body scrolls if it's taller than the window.
            .child(
                div()
                    .id("child-window-body")
                    .size_full()
                    .overflow_y_scroll()
                    .p_4()
                    .child(body),
            )
            .into_any_element()
    }
}

/// Opens a child window titled `title` at `bounds` (screen coordinates), with
/// content rendered by `body` against `host`. Returns the window handle (for
/// focusing / retitling / closing) and the content entity — observe the
/// content's release to learn the user closed the window.
///
/// `parent_display` is the display the parent (chat) window is on. ⚠️ It must
/// be passed through to `WindowOptions`: with no display id, gpui validates the
/// requested bounds against the *primary* monitor and, when their center isn't
/// on it (chat window on a secondary monitor), silently discards them for that
/// display's `default_bounds()` — the "opens as a big window in the wrong
/// place" bug. The center hit-test below also handles the parent straddling
/// two monitors (the child's center may be on the neighbor).
pub fn open<H: Render>(
    title: &str,
    bounds: Bounds<Pixels>,
    min_size: Size<Pixels>,
    parent_display: Option<DisplayId>,
    host: Entity<H>,
    body: impl Fn(&mut H, &mut gpui::Context<H>) -> AnyElement + 'static,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<ChildWindow<H>>)> {
    open_impl(title, bounds, min_size, parent_display, host, body, false, cx)
}

#[allow(clippy::too_many_arguments)]
fn open_impl<H: Render>(
    title: &str,
    bounds: Bounds<Pixels>,
    min_size: Size<Pixels>,
    parent_display: Option<DisplayId>,
    host: Entity<H>,
    body: impl Fn(&mut H, &mut gpui::Context<H>) -> AnyElement + 'static,
    bare: bool,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<ChildWindow<H>>)> {
    let display_id = resolve_display(bounds, parent_display, cx);
    let mut content = None;
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id,
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(title.to_string())),
                ..Default::default()
            }),
            window_min_size: Some(min_size),
            is_minimizable: false,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| ChildWindow {
                host: host.downgrade(),
                body: Box::new(body),
                bare,
                _observe_host: cx.observe(&host, |_, _, cx| cx.notify()),
            });
            content = Some(view.clone());
            // The kit's Root supplies each window's tooltip/popover layers.
            cx.new(|cx| Root::new(view, window, cx))
        },
    )?;
    Ok((handle.into(), content.expect("build_root_view always runs")))
}
