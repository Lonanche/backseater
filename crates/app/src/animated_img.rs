//! An `img()` replacement for chat emotes/badges that owns its animation clock.
//!
//! gpui's `img()` element freezes GIF/WebP animation while the window is unfocused
//! and drives frames by calling `request_animation_frame()` every layout pass —
//! which pins the whole window to the display refresh rate (60–144fps) for a
//! ~10fps emote, making the OS window-move loop stutter under many animated
//! emotes (gpui repaints the whole window; there's no partial-rect repaint, so the
//! repaint *rate* is the only lever). This element does neither: it paints one
//! frame of the cached [`RenderImage`] via
//! `Window::paint_image(.., frame_index, ..)` and schedules its own repaints — a
//! one-shot timer at the animation's real cadence, quantized to a shared ~20ms
//! grid so every animated emote wakes on
//! the *same* ticks. The timer is **one per view per tick**, not one per element:
//! all animated images in a view share a single pending wakeup
//! ([`schedule_wakeup`]) — N per-element timers would each fire their own
//! notify, thousands of main-thread wakeups per second in an emote-heavy chat,
//! which stuttered the OS window-move loop.
//!
//! Images load through the app-wide [`crate::image_cache::LruImageCache`] (LRU
//! eviction + disk-backed bytes); the load call inside `request_layout` is also
//! what stamps the image's "last drawn" time for eviction. Interactivity (click,
//! hover tooltip) stays on wrapper divs at the call sites, as it did with `img()`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{
    px, App, Bounds, Corners, Element, ElementId, EntityId, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, Pixels, RenderImage, SharedString, Style, Window,
};

/// Every animated image wakes on multiples of this tick measured from a shared
/// process epoch (50fps cap, like Chatterino's 20ms global GIF timer), so
/// concurrent animations repaint together instead of at staggered moments.
const ANIM_TICK: Duration = Duration::from_millis(20);

/// The shared process epoch the tick grid is measured from.
fn epoch(now: Instant) -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(|| now)
}

/// The first grid tick covering `now + delay` — but never sooner than one full
/// tick out, so a zero-delay GIF still ticks at the grid rate. Returns the
/// tick's index (the coalescing key) and its absolute deadline.
fn next_tick(now: Instant, delay: Duration) -> (u64, Instant) {
    let epoch = epoch(now);
    let tick = ANIM_TICK.as_nanos().max(1);
    let target = (now + delay).saturating_duration_since(epoch).as_nanos();
    let floor = (now + ANIM_TICK).saturating_duration_since(epoch).as_nanos();
    let index = target.max(floor).div_ceil(tick) as u64;
    (index, epoch + Duration::from_nanos(index * tick as u64))
}

/// The earliest pending wakeup tick per view, so one timer serves every
/// animated image in a view (see [`schedule_wakeup`]).
fn pending_wakeups() -> &'static Mutex<HashMap<EntityId, u64>> {
    static PENDING: OnceLock<Mutex<HashMap<EntityId, u64>>> = OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// Requests a repaint of the current view after `delay`, snapped to the shared
/// tick grid and **coalesced per view**: if an equal-or-earlier wakeup is
/// already pending for this view, this is a no-op; an earlier request
/// supersedes a later pending one (whose timer no-ops when it fires and finds
/// itself replaced). Without this, every animated image detached its own timer
/// on every layout pass — N images × 50fps notify tasks, all for the same
/// repaint.
///
/// The timer task is app-level (not window-bound) so it always runs and clears
/// its pending entry even if the window closes first — a stale entry under a
/// reused `EntityId` would silently swallow that view's animation wakeups.
fn schedule_wakeup(window: &Window, cx: &mut App, delay: Duration) {
    let entity = window.current_view();
    let now = Instant::now();
    let (tick, deadline) = next_tick(now, delay);
    {
        use std::collections::hash_map::Entry;
        let mut pending = pending_wakeups().lock().unwrap();
        match pending.entry(entity) {
            Entry::Occupied(mut slot) => {
                if *slot.get() <= tick {
                    return;
                }
                slot.insert(tick);
            }
            Entry::Vacant(slot) => {
                slot.insert(tick);
            }
        }
    }
    let timer = cx
        .background_executor()
        .timer(deadline.saturating_duration_since(now));
    cx.spawn(async move |cx| {
        timer.await;
        let due = {
            let mut pending = pending_wakeups().lock().unwrap();
            if pending.get(&entity) == Some(&tick) {
                pending.remove(&entity);
                true
            } else {
                false // superseded by an earlier wakeup for this view
            }
        };
        if due {
            cx.update(|cx| cx.notify(entity));
        }
    })
    .detach();
}

/// An animated (or static) image `height` px tall, width following the image's
/// aspect ratio. The `id` must be stable across frames — it keys the per-element
/// frame state (which frame the animation is on).
pub fn animated_img(
    id: impl Into<ElementId>,
    url: impl Into<SharedString>,
    height: Pixels,
) -> AnimatedImage {
    AnimatedImage {
        id: id.into(),
        url: url.into(),
        height,
        max_width: None,
    }
}

/// See [`animated_img`].
pub struct AnimatedImage {
    id: ElementId,
    url: SharedString,
    height: Pixels,
    max_width: Option<Pixels>,
}

impl AnimatedImage {
    /// Clamps the width, shrinking the whole image proportionally to fit
    /// (object-fit: contain) — for fixed-width grid cells (the picker).
    pub fn max_w(mut self, width: Pixels) -> Self {
        self.max_width = Some(width);
        self
    }
}

/// Per-element animation state, persisted across frames under the element id.
struct AnimState {
    frame_index: usize,
    last_frame_time: Option<Instant>,
}

/// Layout → paint handoff: the resolved image + which frame to draw.
pub struct AnimatedImageLayout {
    image: Option<(Arc<RenderImage>, usize)>,
}

impl Element for AnimatedImage {
    type RequestLayoutState = AnimatedImageLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Loading, or failed (the cache logs failures + retries after a cooldown)
        // → render an empty box; the next repaint after the load lands fills it
        // (the cache notifies the view when a spawned load completes).
        let data = match crate::image_cache::load_image(&self.url, window, cx) {
            Some(Ok(data)) => Some(data),
            _ => None,
        };

        let image = window.with_optional_element_state(global_id, |state, window| {
            let mut state = state.map(|state| {
                state.unwrap_or(AnimState {
                    frame_index: 0,
                    last_frame_time: None,
                })
            });
            let image = data.map(|data| {
                let frame_count = data.frame_count();
                let mut frame_index = 0;
                let mut next_frame_in: Option<Duration> = None;
                if let Some(state) = &mut state {
                    state.frame_index = state.frame_index.min(frame_count.saturating_sub(1));
                    if frame_count > 1 {
                        // Advance by the current frame's delay, carrying the
                        // overshoot forward so cadence stays true across jittery
                        // repaints (same math as gpui's img()).
                        let now = Instant::now();
                        let frame_duration = Duration::from(data.delay(state.frame_index));
                        match state.last_frame_time {
                            Some(last) => {
                                let elapsed = now - last;
                                if elapsed >= frame_duration {
                                    state.frame_index = (state.frame_index + 1) % frame_count;
                                    state.last_frame_time = Some(now - (elapsed - frame_duration));
                                    next_frame_in =
                                        Some(Duration::from(data.delay(state.frame_index)));
                                } else {
                                    next_frame_in = Some(frame_duration - elapsed);
                                }
                            }
                            None => {
                                state.last_frame_time = Some(now);
                                next_frame_in = Some(frame_duration);
                            }
                        }
                    } else {
                        state.last_frame_time = None;
                    }
                    frame_index = state.frame_index;
                }
                // One-shot repaint of this view when the next frame is due,
                // regardless of window focus — coalesced with every other
                // animated image in the view (see [`schedule_wakeup`]).
                if let Some(delay) = next_frame_in {
                    schedule_wakeup(window, cx, delay);
                }
                (data, frame_index)
            });
            (image, state)
        });

        // Height is fixed; width follows the current frame's aspect ratio (zero
        // while loading, like an `img()` with only a height). All frames of an
        // animation share a size, so the box is stable across frames.
        let mut size = gpui::Size {
            width: px(0.),
            height: self.height,
        };
        if let Some((data, frame_index)) = &image {
            let frame = data.size(*frame_index);
            if frame.height.0 > 0 {
                let aspect = frame.width.0 as f32 / frame.height.0 as f32;
                size.width = self.height * aspect;
                if let Some(max_w) = self.max_width {
                    if size.width > max_w {
                        size.height = self.height * (max_w / size.width);
                        size.width = max_w;
                    }
                }
            }
        }
        let mut style = Style::default();
        style.size.width = size.width.into();
        style.size.height = size.height.into();
        let layout_id = window.request_layout(style, None, cx);
        (layout_id, AnimatedImageLayout { image })
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _ = cx;
        if let Some((data, frame_index)) = request_layout.image.take() {
            if let Err(err) =
                window.paint_image(bounds, Corners::default(), data, frame_index, false)
            {
                tracing::debug!("animated image paint failed for {}: {err:#}", self.url);
            }
        }
    }
}

impl IntoElement for AnimatedImage {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}
