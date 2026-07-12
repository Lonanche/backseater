//! Clears gpui's stuck hover state when the pointer leaves a window.
//!
//! gpui derives every element's hover from `Window::mouse_position`, re-hit-
//! testing it each frame — but on Windows the pointer leaving the window only
//! fires `WM_MOUSELEAVE`, which flips the window's hovered *flag*
//! (`is_window_hovered`) and refreshes; no input event moves the tracked
//! position. The last in-window coordinate keeps hit-testing forever, so
//! whatever sat under it stays "hovered": row tints, tooltips, `on_hover`
//! listeners never told the mouse left. The fix: every window root calls
//! [`clear`] from its render (the leave's refresh guarantees one runs); when
//! the window has lost hover while the stale position still lies inside it,
//! dispatch one synthetic mouse move far outside so the next hit test finds
//! nothing and every hover listener gets its `false`.

use gpui::{point, px, App, MouseMoveEvent, PlatformInput, Window};

/// Where the synthetic move parks the pointer: any point no hitbox can
/// contain. Also the marker that a clear already ran (the guard below skips
/// positions outside the viewport), so this never re-dispatches in a loop.
const OUTSIDE: f32 = -9999.;

/// Call at the top of a root view's `render`. Dispatches (deferred, after the
/// current draw) one synthetic out-of-window mouse move when the window is no
/// longer hovered but the tracked mouse position is still inside it.
pub(crate) fn clear(window: &mut Window, cx: &mut App) {
    if window.is_window_hovered() || !position_stale(window) {
        return;
    }
    // While a button is held the OS captures the mouse to this window: moves
    // keep streaming (with real outside coordinates) even past the edge, so
    // nothing goes stale — and a synthetic jump mid-drag would yank whatever
    // is being dragged (text selection, a panel divider) to nowhere.
    if any_mouse_button_down() {
        return;
    }
    window.defer(cx, |window, cx| {
        // Re-check: the pointer may have come back between render and defer.
        if window.is_window_hovered() || !position_stale(window) {
            return;
        }
        let modifiers = window.modifiers();
        window.dispatch_event(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: point(px(OUTSIDE), px(OUTSIDE)),
                pressed_button: None,
                modifiers,
            }),
            cx,
        );
    });
}

/// Whether the tracked mouse position still lies inside the window (= stale
/// once the window itself reports unhovered).
fn position_stale(window: &Window) -> bool {
    let pos = window.mouse_position();
    let size = window.viewport_size();
    pos.x >= px(0.) && pos.y >= px(0.) && pos.x <= size.width && pos.y <= size.height
}

#[cfg(windows)]
fn any_mouse_button_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
    };
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .iter()
        .any(|vk| unsafe { GetKeyState(vk.0 as i32) } as u16 & 0x8000 != 0)
}

#[cfg(not(windows))]
fn any_mouse_button_down() -> bool {
    false
}
