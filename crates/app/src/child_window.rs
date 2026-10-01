//! Shared window placement and lifecycle for panels with their own view state.
//! Create these from a plain App context: opening a window renders it immediately.

use gpui::prelude::*;
use gpui::{
    point, AnyWindowHandle, App, Bounds, DisplayId, Entity, Pixels, Size, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use gpui_component::Root;

/// The parent window's screen bounds and display id, used to position a child
/// window over it (see [`open_owned`] for why the display id must travel along). A
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
/// `parent_display`. ⚠️ This must be passed to `WindowOptions` — see [`open_owned`]
/// for the "opens big in the wrong place" bug that omitting it causes.
pub fn resolve_display(
    bounds: Bounds<Pixels>,
    parent_display: Option<DisplayId>,
    cx: &App,
) -> Option<DisplayId> {
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

/// Opens a panel whose state and inputs belong to its own view and window.
#[allow(clippy::too_many_arguments)]
pub fn open_owned<V: Render + 'static>(
    title: &str,
    size: Size<Pixels>,
    min_size: Size<Pixels>,
    parent: AnyWindowHandle,
    persist: Option<&'static str>,
    build: impl FnOnce(&mut Window, &mut gpui::Context<V>) -> V,
    cx: &mut App,
) -> anyhow::Result<(AnyWindowHandle, Entity<V>)> {
    let (parent_bounds, parent_display) = parent_bounds(parent, cx);
    let bounds = persist
        .and_then(|key| crate::window_state::child_bounds(key, cx))
        .unwrap_or_else(|| centered_on(parent_bounds, size));
    let mut content = None;
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            display_id: resolve_display(bounds, parent_display, cx),
            titlebar: Some(TitlebarOptions {
                title: Some(title.to_owned().into()),
                ..Default::default()
            }),
            window_min_size: Some(min_size),
            is_minimizable: false,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| {
                let target = window.window_handle();
                cx.observe_keystrokes(move |_, ev, window, _| {
                    if window.window_handle() == target
                        && ev.keystroke.key == "escape"
                        && !ev.keystroke.modifiers.modified()
                        && ev.action.is_none()
                    {
                        window.remove_window();
                    }
                })
                .detach();
                if let Some(key) = persist {
                    cx.observe_window_bounds(window, move |_, window, cx| {
                        crate::window_state::child_changed(
                            key,
                            window.window_bounds().get_bounds(),
                            cx,
                        );
                    })
                    .detach();
                }
                build(window, cx)
            });
            content = Some(view.clone());
            cx.new(|cx| Root::new(view, window, cx))
        },
    )?;
    Ok((handle.into(), content.expect("window content initialized")))
}
