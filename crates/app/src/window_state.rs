//! Persisted window geometry: the main window and the usercard window reopen
//! at the position/size the user left them (saved to
//! `<config>/backseater/windows.json`).
//!
//! Observers ([`main_changed`] / [`child_changed`]) record every move/resize
//! into a process-wide snapshot; disk writes are debounced (a drag fires a
//! burst of bounds events) and [`flush`]ed when a window closes so nothing is
//! lost to an in-flight debounce at quit. Saved bounds whose center is no
//! longer on any connected display are ignored at open (monitor unplugged) —
//! the window falls back to its default placement.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use gpui::{px, App, Bounds, Pixels, Point, Size, WindowBounds, WindowOptions};
use gpui_component::TitleBar;
use serde::{Deserialize, Serialize};

/// Smallest the main window may be shrunk to (a hard floor so the tab strip +
/// title bar controls always fit).
const MAIN_MIN_SIZE: Size<Pixels> = Size {
    width: px(480.),
    height: px(320.),
};

const STORE_NAME: &str = "windows";
/// Coalesces the burst of bounds events a drag/resize fires into one write.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(750);

/// One window's saved rect, in the same logical-pixel screen coordinates gpui's
/// `window_bounds()` reports and `WindowOptions::window_bounds` accepts.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedBounds {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl From<Bounds<Pixels>> for SavedBounds {
    fn from(b: Bounds<Pixels>) -> Self {
        Self {
            x: b.origin.x.into(),
            y: b.origin.y.into(),
            w: b.size.width.into(),
            h: b.size.height.into(),
        }
    }
}

impl SavedBounds {
    pub fn to_bounds(self) -> Bounds<Pixels> {
        Bounds {
            origin: Point {
                x: px(self.x),
                y: px(self.y),
            },
            size: Size {
                width: px(self.w),
                height: px(self.h),
            },
        }
    }
}

/// Everything persisted to `windows.json`. `main` holds the main window's
/// *restore* bounds (its windowed rect even while maximized — that's what
/// gpui's `window_bounds()` reports); child windows are keyed by a stable name
/// (today just the usercard).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct WindowStates {
    main: Option<SavedBounds>,
    main_maximized: bool,
    children: HashMap<String, SavedBounds>,
}

static STATE: OnceLock<Mutex<WindowStates>> = OnceLock::new();
/// Bumped per change; a debounced save task only writes if it's still the
/// latest, so a burst schedules many tasks but lands one write.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static DIRTY: AtomicBool = AtomicBool::new(false);

fn state() -> &'static Mutex<WindowStates> {
    STATE.get_or_init(|| {
        Mutex::new(
            bks_auth::store::load(STORE_NAME)
                .ok()
                .flatten()
                .unwrap_or_default(),
        )
    })
}

/// `WindowOptions` for opening the main window: the saved bounds (and maximized
/// state) when they're still on a connected display, else the defaults.
pub fn main_window_options(cx: &mut App) -> WindowOptions {
    let (saved, maximized) = {
        let s = state().lock().unwrap();
        (s.main, s.main_maximized)
    };
    // Transparent OS caption + a min size: the kit `TitleBar` draws our own
    // caption (login status + gear), and on Windows still hands the min/max/close
    // buttons back to the OS via `window_control_area`.
    let base = WindowOptions {
        titlebar: Some(TitleBar::title_bar_options()),
        window_min_size: Some(MAIN_MIN_SIZE),
        ..Default::default()
    };
    let Some(bounds) = saved.map(SavedBounds::to_bounds).filter(|b| on_screen(b, cx)) else {
        return base;
    };
    WindowOptions {
        window_bounds: Some(if maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        }),
        // ⚠️ Without the display id, gpui validates the bounds against the
        // *primary* monitor and silently swaps them for its default bounds
        // when their center is elsewhere (same bug as `child_window::open`).
        display_id: crate::child_window::resolve_display(bounds, None, cx),
        ..base
    }
}

/// The saved rect for the child window `key`, if it's still on a connected
/// display.
pub fn child_bounds(key: &str, cx: &App) -> Option<Bounds<Pixels>> {
    let saved = *state().lock().unwrap().children.get(key)?;
    Some(saved.to_bounds()).filter(|b| on_screen(b, cx))
}

/// Whether a saved rect's center still lies on some connected display.
fn on_screen(bounds: &Bounds<Pixels>, cx: &App) -> bool {
    cx.displays()
        .into_iter()
        .any(|d| d.bounds().contains(&bounds.center()))
}

/// Records the main window's bounds after a move/resize. Takes the full
/// `WindowBounds` so a maximized window saves its restore rect + the flag
/// (reopening restores maximized with the right un-maximize size); fullscreen
/// is transient and saves as windowed at the restore rect.
pub fn main_changed(wb: WindowBounds, cx: &mut App) {
    let (bounds, maximized) = match wb {
        WindowBounds::Windowed(b) => (b, false),
        WindowBounds::Maximized(b) => (b, true),
        WindowBounds::Fullscreen(b) => (b, false),
    };
    let saved = SavedBounds::from(bounds);
    {
        let mut s = state().lock().unwrap();
        if s.main == Some(saved) && s.main_maximized == maximized {
            return;
        }
        s.main = Some(saved);
        s.main_maximized = maximized;
    }
    save_soon(cx);
}

/// Records a child window's bounds under `key` after a move/resize.
pub fn child_changed(key: &'static str, bounds: Bounds<Pixels>, cx: &mut App) {
    let saved = SavedBounds::from(bounds);
    {
        let mut s = state().lock().unwrap();
        if s.children.get(key) == Some(&saved) {
            return;
        }
        s.children.insert(key.to_string(), saved);
    }
    save_soon(cx);
}

fn save_soon(cx: &mut App) {
    DIRTY.store(true, Ordering::Relaxed);
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DEBOUNCE).await;
        if GENERATION.load(Ordering::Relaxed) == generation {
            flush();
        }
    })
    .detach();
}

/// Writes the snapshot to disk now if anything changed since the last write.
/// Called on every window close so a close (or the quit it triggers) can't
/// outrun the debounce.
pub fn flush() {
    if !DIRTY.swap(false, Ordering::Relaxed) {
        return;
    }
    let snapshot = state().lock().unwrap().clone();
    if let Err(err) = bks_auth::store::save(STORE_NAME, &snapshot) {
        tracing::warn!("failed to save window bounds: {err:#}");
    }
}
