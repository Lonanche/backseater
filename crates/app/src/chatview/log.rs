//! The chat-log region as its **own cached child view** of [`ChatView`].
//!
//! Why: `cx.notify(view)` dirties the view *and every ancestor view*
//! (`window.rs::mark_view_dirty`), so a picker `EmoteCell`'s animation tick
//! dirties `ChatView` — and an uncached `ChatView` re-render rebuilds the log's
//! visible rows (hundreds of flex-wrap token elements) at up to 50fps while the
//! picker shows animated emotes. Caching only spares subtrees *off* the dirty
//! path, so the heavy log lives here, as a sibling of the picker: a cell tick
//! re-renders ChatView's (light) chrome while this view's prepaint/paint are
//! reused wholesale. The log's own animated emotes notify *this* view (they
//! render inside it, so `window.current_view()` is the `LogView`), which keeps
//! chat animation working exactly as before.
//!
//! All state stays on `ChatView` (rows, selection, list state, config); this
//! view only *renders* it. That means `ChatView` must explicitly notify this
//! view whenever log content changes — new rows, font size, panel resize, theme —
//! via [`ChatView::refresh_log`] (a `.cached()` view reuses its paint until it
//! is itself dirtied or its bounds change). Scrolling notifies it through the
//! `ListState` scroll handler; bounds changes (window/panel resize, picker
//! open/close) miss the cache key and re-render automatically.

use gpui::prelude::*;
use gpui::{
    div, image_cache, list, px, App, Context, FollowMode, FontWeight, MouseButton, Pixels, Point,
    SharedString, WeakEntity, Window,
};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{h_flex, v_flex, ActiveTheme};

use super::{
    mention_click_for, mention_click_for_platform, mod_click_for, name_click_for,
    name_right_click_for, pin_click_for, reply_click_for, thread_click_for, ChatView,
    EmotePopup, Row,
};
use crate::channel_store::ChannelModel;
use crate::{render, ORDINAL_STRIDE, SCROLLBAR_WIDTH};
use bks_core::Message;

/// A message decorated with its author's resolved 7TV cosmetics for rendering:
/// the shared message untouched when there are none (no copy), else an owned
/// clone with the paint/badge applied. Cosmetics live on the shared model, not
/// baked onto the immutable message, so they apply retroactively + are shared.
pub(super) fn decorate<'a>(msg: &'a Message, model: &ChannelModel) -> std::borrow::Cow<'a, Message> {
    match model.cosmetics_for(msg.platform, &msg.author.user_id) {
        Some(c) => {
            let mut owned = msg.clone();
            crate::apply_cosmetics_to_author(&mut owned.author, c);
            std::borrow::Cow::Owned(owned)
        }
        None => std::borrow::Cow::Borrowed(msg),
    }
}

/// A row's local calendar date, when it has one (messages and events carry a
/// timestamp; system/error/live notices don't and inherit their neighbors').
pub(super) fn row_date(row: &Row) -> Option<chrono::NaiveDate> {
    let ts = match row {
        Row::Message { msg } => msg.timestamp,
        Row::Event { timestamp, .. } => *timestamp,
        _ => return None,
    };
    Some(ts.with_timezone(&chrono::Local).date_naive())
}

/// The divider band's label ("Wednesday, July 16, 2026") — one formatter so the
/// leading dividers and the trailing band can never drift apart.
fn day_label(date: chrono::NaiveDate) -> String {
    date.format("%A, %B %-d, %Y").to_string()
}

/// The day-divider label for the row at `ix`, when it's the first row of a new
/// local calendar day: its date differs from the previous dated row's (scanning
/// back over undated notice rows), or — for the buffer's first dated row — from
/// `today` (today's chat needs no header; an older backlog announces its day).
/// `today` is the view's creation date, like the trailing band's — a live clock
/// here would let midnight flip a row's divider without a re-measure (stale
/// cached height), and would cost a clock read per visible row per frame.
/// Render-derived instead of a buffer row so `rows`/`ListState` lockstep,
/// sorted history insertion, and trimming stay untouched.
fn day_divider_label(
    model: &ChannelModel,
    ix: usize,
    row: &Row,
    today: chrono::NaiveDate,
) -> Option<String> {
    let date = row_date(row)?;
    let prev = model.rows.iter().take(ix).rev().find_map(row_date);
    is_new_day(prev, date, today).then(|| day_label(date))
}

/// The divider rule, separated for testing: a dated row opens a new day when
/// the previous dated row was another day — or, with nothing dated above it,
/// when it isn't from today (today's live chat needs no header; a backlog
/// reaching back announces its day).
fn is_new_day(
    prev: Option<chrono::NaiveDate>,
    date: chrono::NaiveDate,
    today: chrono::NaiveDate,
) -> bool {
    match prev {
        Some(prev) => prev != date,
        None => date != today,
    }
}

/// The trailing "new day started" band under the log's final row: when the
/// newest dated row is from a local day *before* `today`, the calendar has
/// moved on with no message yet — a restarted app whose whole backlog is
/// yesterday's says so immediately instead of waiting for the next message to
/// carry a leading divider. `today` is the date captured when this view was
/// created (NOT a live clock read): the band is deliberately a launch-time
/// affordance, so a session running past midnight doesn't grow one under a
/// row whose cached height never accounted for it. The first message of the
/// new day replaces it (its leading divider takes over — the `Appended`
/// handler re-measures the previously-last row so the stale band can't
/// linger).
/// `item_count` scopes the scan to the rows this *view* shows (a hover-paused
/// view is frozen at a prefix of the model; the band sits under its frozen last
/// row, so it must be computed from that row, not the model's newest).
pub(super) fn trailing_day_label(
    model: &ChannelModel,
    item_count: usize,
    today: chrono::NaiveDate,
) -> Option<String> {
    let last = model.rows.iter().take(item_count).rev().find_map(row_date)?;
    (last < today).then(|| day_label(today))
}


/// Builds the inline link-preview card for a message row, or `None` when inline
/// previews are off, the message has no previewable link, or its fetch failed.
/// A still-loading preview renders a fixed-height skeleton (space reserved up
/// front so the row doesn't jump when the fetch lands — see
/// [`ChatView::arm_inline_preview`]).
fn build_inline_preview(msg: &Message, font_size: f32) -> Option<gpui::AnyElement> {
    if crate::settings::link_preview_mode() != crate::settings::LinkPreviewMode::Inline {
        return None;
    }
    let url = crate::preview::first_previewable_url(msg)?;
    let preview = match crate::preview::peek(&url) {
        crate::preview::PreviewState::Ready(p) => {
            // "channel · views · Clipped by X", each part dropped when absent.
            let meta = [
                p.author.clone(),
                p.stats.clone().unwrap_or_default(),
                p.byline.clone().unwrap_or_default(),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
            // Streamer mode can hide the thumbnail (it can reveal what a posted
            // link points at on stream); the rest of the card still shows.
            let hidden = crate::settings::hide_preview_thumbnails();
            let thumbnail_url = if hidden {
                None
            } else {
                p.thumbnail_url.clone().map(SharedString::from)
            };
            render::InlinePreview {
                title: SharedString::from(p.title.clone()),
                meta: SharedString::from(meta),
                thumbnail_url,
                thumbnail_hidden: hidden,
                url,
            }
        }
        crate::preview::PreviewState::Loading => render::InlinePreview {
            title: SharedString::from("Loading preview…"),
            meta: SharedString::default(),
            thumbnail_url: None,
            thumbnail_hidden: false,
            url,
        },
        // Failed / unsupported → no card (the reserved space collapses).
        crate::preview::PreviewState::None => return None,
    };
    Some(render::inline_preview_card(preview, &msg.id, font_size))
}

pub(super) struct LogView {
    host: WeakEntity<ChatView>,
    /// The local date this view was created — the reference "today" for the
    /// trailing new-day band (see [`trailing_day_label`]).
    today: chrono::NaiveDate,
}

impl LogView {
    pub(super) fn new(host: WeakEntity<ChatView>) -> Self {
        Self {
            host,
            today: chrono::Local::now().date_naive(),
        }
    }
}

/// The style for the cached `AnyView` node wrapping the log. A cached view's
/// layout node is built from this refinement alone (its content isn't measured),
/// so it must carry the flex participation the old inline container had:
/// `flex_1` + `min_w_0` + `min_h_0`, filling the row beside the events panel.
/// The cache key includes the resolved bounds, so a resize re-renders correctly.
pub(super) fn log_view_style() -> gpui::StyleRefinement {
    let mut s = gpui::StyleRefinement {
        flex_grow: Some(1.),
        flex_shrink: Some(1.),
        flex_basis: Some(px(0.).into()),
        ..Default::default()
    };
    s.min_size.width = Some(px(0.).into());
    s.min_size.height = Some(px(0.).into());
    s
}

impl Render for LogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(host) = self.host.upgrade() else {
            return div().into_any_element();
        };
        let (font_size, selection, focus, list_state, log_image_cache, hide_events_in_log, model) = {
            let h = host.read(cx);
            (
                h.font_size,
                h.selection.clone(),
                h.focus.clone(),
                h.list_state.clone(),
                h.image_cache.clone(),
                h.config.layout.contains(crate::tabs::PanelKind::Events) && h.config.events_only,
                h.channel.clone(),
            )
        };
        let paused = host.read(cx).log_paused;

        // Rebuild the per-frame token registry. This runs only when the log
        // actually re-renders; while the cached paint is reused the registry
        // keeps the previous frame's tokens — whose bounds are still exact,
        // since an unchanged cache key means nothing moved.
        selection.begin_frame();

        // Repaints the log when a link's hover state changes (shared across all
        // pieces of a wrapped link so the whole link underlines together). Must
        // notify *this* view: the underline paints inside these rows, and a
        // notify on ChatView alone would not dirty this cached child.
        let link_hover: render::LinkHover = {
            let log = cx.entity();
            std::rc::Rc::new(move |cx: &mut App| {
                log.update(cx, |_, cx| cx.notify());
            })
        };
        // Hovering a previewable link (YouTube today) arms/disarms the preview
        // tooltip on the host ChatView (which renders the overlay). A no-op unless
        // previews are in Tooltip mode — decided inside `on_link_preview_hover`.
        let link_preview_hover: render::LinkPreviewHover = {
            let host = host.clone();
            std::rc::Rc::new(
                move |url: &str,
                      entered: bool,
                      pos: Point<Pixels>,
                      _window: &mut Window,
                      cx: &mut App| {
                    let url = url.to_string();
                    host.update(cx, |this, cx| {
                        this.on_link_preview_hover(&url, entered, pos, cx);
                    });
                },
            )
        };
        // Clicking an emote opens its info popup at the click position (the
        // popup renders as a ChatView overlay, so that's who gets notified).
        let emote_click: render::EmoteClick = {
            let host = host.clone();
            std::rc::Rc::new(
                move |emote: &bks_core::Emote,
                      pos: Point<Pixels>,
                      _window: &mut Window,
                      cx: &mut App| {
                    let popup = EmotePopup::from_emote(emote, pos);
                    host.update(cx, |this, cx| {
                        this.emote_popup = Some(popup);
                        cx.notify();
                    });
                },
            )
        };
        // A held AutoMod row's Allow/Deny: forward to the controller (the row
        // resolves itself when the EventSub update comes back).
        let automod_click: render::AutoModClick = {
            let host = host.clone();
            std::rc::Rc::new(
                move |message_id: &str, allow: bool, _window: &mut Window, cx: &mut App| {
                    let message_id = message_id.to_string();
                    host.update(cx, |this, _| {
                        this.automod_action(message_id, allow);
                    });
                },
            )
        };
        // Clicking a 7TV emote *link* in chat fetches the emote by id, then opens
        // the same popup. A loading placeholder shows immediately so the click
        // feels responsive while the REST lookup runs.
        let seventv_link_click: render::SeventvLinkClick = {
            let host = host.clone();
            std::rc::Rc::new(
                move |id: &str, pos: Point<Pixels>, _window: &mut Window, cx: &mut App| {
                    let id = id.to_string();
                    host.update(cx, |this, cx| {
                        this.open_seventv_link(id, pos, cx);
                    });
                },
            )
        };

        let render_entity = host.clone();
        let render_model = model.clone();
        // The final row is spotted by this list's own count (not
        // `model.rows.len()`), so a hover-paused view keeps the trailing day
        // band on *its* frozen last row while the model grows past it. Read
        // ONCE here, before the list element runs: the item closure executes
        // while the list holds its state RefCell mutably, so calling
        // `item_count()` from inside it panics ("already mutably borrowed").
        // The value can't go stale mid-layout — every splice path calls
        // `refresh_log`, re-rendering this view before the next layout.
        let item_count = list_state.item_count();
        let session_today = self.today;
        let chat_list = list(list_state.clone(), move |ix, _window, cx: &mut App| {
            let this = render_entity.read(cx);
            let model = render_model.read(cx);
            let Some(row) = model.rows.get(ix) else {
                return div().into_any_element();
            };
            // Ordinals are derived from the row index (not a running counter),
            // so a selection's endpoints stay valid as the visible window
            // shifts; the stride leaves room for every token in a row.
            let mut ordinal = ix * ORDINAL_STRIDE;
            // The first row of a new local calendar day carries a date band
            // above itself (render-derived — no divider rows in the buffer);
            // the view's final row also carries a trailing "new day started"
            // band when the calendar has moved past the newest message.
            let divider = day_divider_label(model, ix, row, session_today);
            let trailing = (ix + 1 == item_count)
                .then(|| trailing_day_label(model, item_count, session_today))
                .flatten();
            // A divider-only stand-in for rows that render nothing themselves:
            // without it, hiding the day's first (or last) row would hide the
            // day band(s).
            let divider_only = |divider: &Option<String>, trailing: &Option<String>| {
                if divider.is_none() && trailing.is_none() {
                    return div().into_any_element();
                }
                div()
                    .w_full()
                    .min_w_0()
                    .when_some(divider.as_deref(), |w, label| {
                        w.child(render::render_day_divider(label, font_size))
                    })
                    .when_some(trailing.as_deref(), |w, label| {
                        w.child(render::render_day_divider(label, font_size))
                    })
                    .into_any_element()
            };
            // Special rows carry a `(tint, accent bar)` pair the wrapper below
            // paints full-bleed across the log's width; the row content itself
            // renders bare (see `RowFlags::external_highlight` / the renderers'
            // panel modes).
            let mut highlight: Option<(u32, u32)> = None;
            // A jumped-to (clicked-mention) message row flashes to catch the eye:
            // a fading translucent overlay strength in [0,1], applied over the
            // row's base tint in the wrapper below.
            let mut flash: Option<f32> = None;
            // Set (to the message id) on rows that must track pointer hover for
            // the "On hover" mod-button mode; the wrapper below then carries an
            // `on_hover` listener that shows/hides the strip via the view.
            let mut hover_track: Option<String> = None;
            let inner = match row {
                Row::Message { msg } => {
                    // This view's own filters (shared buffer, per-view display):
                    // an ignored message renders as an empty (height-0) row so the
                    // list stays index-aligned with the shared model; mention tint
                    // is this view's terms.
                    if this.ignore.matches_message(msg) {
                        return divider_only(&divider, &trailing);
                    }
                    let mentioned = this.mentions.matches(&msg.raw_text);
                    flash = this.flash_strength_for(msg.platform, &msg.id);
                    highlight = if mentioned {
                        Some(render::highlight_mention())
                    } else if msg.first_message {
                        Some(render::highlight_first_message())
                    } else if msg.highlighted {
                        Some(render::highlight_highlighted())
                    } else {
                        None
                    };
                    let name_click = name_click_for(&render_entity, msg);
                    let name_right_click = name_right_click_for(&render_entity, msg);
                    let reply_click = reply_click_for(&render_entity, msg);
                    // A reply's context line is clickable to open the thread panel;
                    // non-reply rows have no context line, so no handler.
                    let thread_click = msg
                        .reply
                        .is_some()
                        .then(|| thread_click_for(&render_entity, msg));
                    // The 📌 button only renders for Twitch rows the user can
                    // moderate that carry a real platform message id (not a
                    // local echo) — pinning is Helix-only; Kick's site API
                    // rejects third-party tokens.
                    let can_moderate = model.can_moderate(msg.platform);
                    let pin_click = (msg.platform == bks_core::Platform::Twitch
                        && can_moderate
                        && !msg.id.starts_with("echo-"))
                    .then(|| pin_click_for(&render_entity, msg));
                    // The left-side mod-button strip. Always mode gives EVERY
                    // message row the same-width gutter when this view
                    // moderates anything (rows whose platform isn't moderated
                    // get all-ghost slots), so a merged feed's messages stay
                    // horizontally aligned; Hover renders it only on the
                    // moderatable row under the pointer (tracked on the view
                    // via the wrapper's `on_hover`, so every frame is laid out
                    // consistently — a group-hover display switch panicked
                    // when hover flipped between prepaint and paint).
                    let mod_platforms = model.mod_platforms();
                    let strip_shown = !mod_platforms.is_empty()
                        && match crate::settings::mod_button_mode() {
                            crate::settings::ModButtonMode::Off => false,
                            crate::settings::ModButtonMode::Always => true,
                            crate::settings::ModButtonMode::Hover => {
                                can_moderate && {
                                    hover_track = Some(msg.id.clone());
                                    this.hover_strip_row.as_deref() == Some(msg.id.as_str())
                                }
                            }
                        };
                    let mod_strip = strip_shown.then(|| render::ModStrip {
                        click: mod_click_for(&render_entity, msg),
                        platforms: mod_platforms,
                        row_moderated: can_moderate,
                    });
                    // The inline link-preview card (Inline mode + a previewable
                    // link): a fixed-height skeleton reserves its space up front,
                    // filled in when the fetch (armed on append) lands.
                    let inline_preview = build_inline_preview(msg, font_size);
                    // Struck (ban/delete) + cosmetics come from the shared model's
                    // side-tables, not baked onto the immutable message.
                    let struck = model.is_struck(msg);
                    let decorated = decorate(msg, model);
                    render::render_message(
                        &decorated,
                        render::RowFlags {
                            struck,
                            mentioned,
                            external_highlight: true,
                            hide_timestamp: !crate::settings::show_timestamps_chat(),
                            suppressed: this.suppress.matches_message(msg),
                        },
                        font_size,
                        &this.selection,
                        &mut ordinal,
                        render::RowHandlers {
                            name_click: Some(name_click),
                            name_right_click: Some(name_right_click),
                            mention_click: Some(mention_click_for(&render_entity, msg)),
                            link_hover: Some(link_hover.clone()),
                            link_preview_hover: Some(link_preview_hover.clone()),
                            emote_click: Some(emote_click.clone()),
                            seventv_link_click: Some(seventv_link_click.clone()),
                            reply_click: Some(reply_click),
                            thread_click,
                            pin_click,
                            mod_strip,
                            inline_preview,
                        },
                    )
                    .into_any_element()
                }
                Row::System(text) => render::render_system(text, font_size).into_any_element(),
                Row::Error(text) => {
                    highlight = Some(render::highlight_error());
                    render::render_error(text, font_size, &this.selection, &mut ordinal)
                        .into_any_element()
                }
                Row::Event { .. } if hide_events_in_log => {
                    return divider_only(&divider, &trailing)
                }
                Row::Event {
                    platform,
                    kind,
                    text,
                    timestamp,
                    message,
                    accent,
                    actor,
                    ..
                } => {
                    highlight = Some(render::highlight_event(*kind, *accent));
                    let mention_click = mention_click_for_platform(&render_entity, *platform);
                    render::render_event(
                        *platform,
                        *kind,
                        text,
                        // Event rows share the chat log's timestamp toggle.
                        crate::settings::show_timestamps_chat().then_some(*timestamp),
                        message.as_deref(),
                        *accent,
                        actor.as_deref(),
                        Some(&mention_click),
                        font_size,
                        false,
                    )
                    .into_any_element()
                }
                Row::Live {
                    platform,
                    live,
                    title,
                } => {
                    highlight = Some(render::highlight_live(*live));
                    render::render_live(*platform, *live, title, font_size).into_any_element()
                }
                Row::AutoMod {
                    message_id,
                    user,
                    text,
                    reason,
                    resolved,
                } => {
                    highlight = Some(render::highlight_automod());
                    // AutoMod rows are Twitch-only (the EventSub feed they arrive
                    // on is Twitch's), so the chatter name opens a Twitch usercard.
                    let name_click = mention_click_for_platform(
                        &render_entity,
                        bks_core::Platform::Twitch,
                    );
                    render::render_automod(
                        message_id,
                        user,
                        text,
                        reason,
                        resolved.as_ref().map(|(s, m)| (*s, m.as_str())),
                        font_size,
                        &this.selection,
                        &mut ordinal,
                        automod_click.clone(),
                        Some(&name_click),
                    )
                    .into_any_element()
                }
            };
            // Per-row bottom gap (the list lays rows back-to-back; it has no
            // flex `gap`), kept *outside* the tinted box so highlights hug their
            // row. The inner box owns the shared insets: every row carries the
            // left accent border (transparent when un-highlighted, so text never
            // shifts) and the right gutter that keeps content clear of the
            // scrollbar thumb — while a highlight's tint spans the wrapper's
            // full width, edge-to-edge under the thumb. `pr` is applied per row
            // (not on the list, where `Auto` sizing would clip rather than
            // reflow).
            // A historical (backlog-replayed) event fades like a historical
            // chat line; the fade sits on the tinted box so the day band above
            // it stays full-strength. (Message rows fade inside their renderer.)
            let event_faded = matches!(
                row,
                Row::Event {
                    historical: true,
                    ..
                }
            );
            let wrapper = div()
                .w_full()
                .min_w_0()
                .pb_1()
                .when_some(divider, |w, label| {
                    w.child(render::render_day_divider(&label, font_size))
                })
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .pl(px(10.))
                        .pr(px(SCROLLBAR_WIDTH))
                        .border_l_2()
                        .border_color(gpui::transparent_black())
                        .when(event_faded, |row| row.opacity(render::HISTORY_OPACITY))
                        .map(|row| match (highlight, flash) {
                            // Flashing (jumped-to) row: the flash tint is blended
                            // over whatever base the row has (a highlight's tint
                            // or the plain log bg), fading with `strength`. It
                            // borrows the mention accent bar so it reads as
                            // "here" while lit.
                            (base, Some(strength)) => {
                                let base_bg = base.map_or_else(render::chat_bg, |(bg, _)| bg);
                                let accent = base
                                    .map(|(_, a)| a)
                                    .unwrap_or_else(|| render::highlight_mention().1);
                                row.py_0p5()
                                    .bg(gpui::rgb(render::flash_over(base_bg, strength)))
                                    .border_color(gpui::rgb(accent))
                            }
                            (Some((bg, accent)), None) => row
                                .py_0p5()
                                .bg(gpui::rgb(bg))
                                .border_color(gpui::rgb(accent)),
                            // A whisper of tint while the pointer is over the
                            // row, so the row under the cursor (and its hover
                            // actions) reads at a glance.
                            (None, None) => row.hover(|s| s.bg(render::row_hover())),
                        })
                        .child(inner),
                )
                // The trailing "new day started" band sits under the final
                // row's content, outside its tint/fade.
                .when_some(trailing, |w, label| {
                    w.child(render::render_day_divider(&label, font_size))
                });
            // "On hover" mod-button mode: this row shows/hides its strip by
            // hover, tracked on the view (`hover_strip_row`) so the next log
            // render adds/removes the strip — never a same-frame style switch.
            // Leaving one row and entering another fires leave-then-enter in
            // hitbox order; the guards keep the pair from clobbering each other.
            match hover_track {
                Some(msg_id) => {
                    let entity = render_entity.clone();
                    wrapper
                        .id(("mod-hover", ix))
                        .on_hover(move |hovered, _window, cx| {
                            entity.update(cx, |this, cx| {
                                if *hovered {
                                    if this.hover_strip_row.as_deref() != Some(msg_id.as_str()) {
                                        this.hover_strip_row = Some(msg_id.clone());
                                        this.refresh_log(cx);
                                    }
                                } else if this.hover_strip_row.as_deref() == Some(msg_id.as_str())
                                {
                                    this.hover_strip_row = None;
                                    this.refresh_log(cx);
                                }
                            });
                        })
                        .into_any_element()
                }
                None => wrapper.into_any_element(),
            }
        })
        // `Auto` sizing lays the list out with a bare default style, so it must be
        // told to fill its parent or it collapses to zero height (no rows visible).
        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
        .size_full();

        let sel_down = selection.clone();
        let sel_move = selection.clone();
        let sel_up = selection.clone();
        let sel_copy = selection.clone();
        let host_down = host.clone();

        v_flex()
            .id("chat-log")
            .track_focus(&focus)
            // Fill the bounds the cached wrapper node resolved (the flex math
            // lives on that node — see [`log_view_style`]); `min_*_0` keep the
            // list scrolling inside instead of growing the panel.
            .size_full()
            .min_w_0()
            .min_h_0()
            .relative() // anchors the absolutely-positioned "jump to latest" pill.
            // Vertical padding only: each row owns its horizontal insets (see the
            // list closure), so a highlighted row's tint can bleed edge-to-edge —
            // the right side stays clear for the overlay scrollbar the same way.
            .pt_2()
            .pb_2()
            // Slightly lighter than the window background so dark usernames (also
            // contrast-fixed in `render`) read better — the reference the name
            // contrast-fix lightens against.
            .bg(gpui::rgb(render::chat_bg()))
            // Child rows inherit this; the size is an app preference.
            .text_size(px(font_size))
            // Drag-to-select: down starts, move extends, up ends. Focus
            // the log on down so Ctrl/Cmd+C reaches the copy handler.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, ev: &gpui::MouseDownEvent, window, cx| {
                    host_down.update(cx, |this, cx| {
                        this.focus.focus(window, cx);
                    });
                    sel_down.start(ev.position);
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(move |_, ev: &gpui::MouseMoveEvent, _window, cx| {
                    if sel_move.is_selecting() && sel_move.extend(ev.position) {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |_, _ev: &gpui::MouseUpEvent, _window, _cx| {
                    sel_up.finish();
                }),
            )
            .on_key_down(cx.listener(move |_, ev: &gpui::KeyDownEvent, _window, cx| {
                let ks = &ev.keystroke;
                if ks.key == "c" && (ks.modifiers.control || ks.modifiers.platform) {
                    let text = sel_copy.selected_text();
                    if !text.is_empty() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    }
                }
            }))
            // The virtualized list fills the remaining height and scrolls itself;
            // `flex_1` + `min_h_0` give it a definite height to lay rows against.
            // `relative` so the scrollbar (absolute, fills its parent) overlays the
            // list; it drives the same `ListState`, so dragging it scrolls the log.
            .child(
                // `vertical_scrollbar` overlays an absolute, full-bleed scrollbar
                // bound to the list's `ListState` (so dragging it scrolls the log);
                // it must sit on a `relative` parent the same size as the list.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    // Route the log's emote/badge images through the scoped cache so
                    // the eviction sweep can free off-screen emotes' decoded frames.
                    .child(image_cache(log_image_cache).size_full().child(chat_list))
                    // The scrollbar mounts only while scrolled off the bottom:
                    // tail-following, every appended message moves the offset,
                    // which the kit's fade logic counts as scrolling — the bar
                    // never rested while chat was active. Wheeling up disengages
                    // tail-follow (a scroll re-render), so the bar appears
                    // exactly when there's history to navigate.
                    .when(!list_state.is_following_tail(), |d| {
                        d.vertical_scrollbar(&list_state)
                    }),
            )
            // When scrolled up off the bottom, show a "jump to latest" pill;
            // while hover-paused at the bottom, a "paused" pill instead.
            .children(self.jump_to_latest(&list_state, cx))
            .children(self.paused_pill(paused, &list_state, cx))
            // A transient note when a clicked mention can't be jumped to (aged
            // out of the buffer).
            .children(self.jump_note_pill(&host, cx))
            .into_any_element()
    }
}

impl LogView {
    /// The "jump to latest" pill, shown only while the log is scrolled up off the
    /// bottom (not following the tail). Clicking it snaps back to the newest row,
    /// which re-engages tail-follow. Absolutely positioned, centered over the log's
    /// bottom edge (its container is `relative`).
    fn jump_to_latest(
        &self,
        list_state: &gpui::ListState,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if list_state.is_following_tail() {
            return None;
        }
        let list_state = list_state.clone();
        // A full-width, non-interactive overlay row pinned to the bottom that just
        // centers the pill; only the pill itself is clickable.
        Some(
            div()
                .absolute()
                .bottom_2()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .h_7()
                        .pl_3()
                        .pr_2p5()
                        .rounded_full()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .text_color(cx.theme().popover_foreground)
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .shadow_lg()
                        .cursor_pointer()
                        .hover(|s| s.bg(cx.theme().accent).border_color(cx.theme().accent))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_, _ev: &gpui::MouseDownEvent, _window, cx| {
                                // Re-engage tail-follow (scrolls to the end *and*
                                // resumes sticking to the bottom, hiding the pill).
                                list_state.set_follow_mode(FollowMode::Tail);
                                cx.notify();
                            }),
                        )
                        .child(SharedString::from("Jump to latest"))
                        .child(
                            // A small circular badge holding the down chevron.
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .size_4()
                                .rounded_full()
                                .bg(cx.theme().muted)
                                .child(SharedString::from("↓")),
                        ),
                )
                .into_any_element(),
        )
    }

    /// A transient centered note shown when a clicked mention can't be jumped to
    /// (its message has aged out of the buffer). Non-interactive; it fades on the
    /// view's flash tick (`ChatView::jump_note` returns `None` once expired).
    /// Sits above where the "jump to latest" pill would be so the two don't stack.
    fn jump_note_pill(
        &self,
        host: &gpui::Entity<ChatView>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let text = host.read(cx).jump_note()?;
        Some(
            div()
                .absolute()
                .bottom_12()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    h_flex()
                        .items_center()
                        .h_7()
                        .px_3()
                        .rounded_full()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .text_color(cx.theme().muted_foreground)
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .shadow_lg()
                        .child(text),
                )
                .into_any_element(),
        )
    }

    /// A small non-interactive "Chat paused" pill while hover-pause holds the
    /// log still. Shown only while still tail-following — a mid-pause manual
    /// scroll shows the "jump to latest" pill instead (they'd overlap).
    fn paused_pill(
        &self,
        paused: bool,
        list_state: &gpui::ListState,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !paused || !list_state.is_following_tail() {
            return None;
        }
        Some(
            div()
                .absolute()
                .bottom_2()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1p5()
                        .h_7()
                        .px_3()
                        .rounded_full()
                        .bg(cx.theme().popover)
                        .border_1()
                        .border_color(cx.theme().border)
                        .text_color(cx.theme().popover_foreground)
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .shadow_lg()
                        .child(
                            // The kit-shipped lucide pause icon. gpui's `svg()`
                            // paints only with a text color set on the svg
                            // element itself (nothing cascades from the pill).
                            gpui::svg()
                                .path("icons/pause.svg")
                                .size(px(12.))
                                .flex_none()
                                .text_color(cx.theme().popover_foreground),
                        )
                        .child(SharedString::from("Chat paused")),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::is_new_day;
    use chrono::NaiveDate;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, day).unwrap()
    }

    #[test]
    fn divider_opens_when_the_date_changes() {
        assert!(is_new_day(Some(d(15)), d(16), d(16)));
        assert!(!is_new_day(Some(d(16)), d(16), d(16)));
    }

    #[test]
    fn first_dated_row_announces_only_a_past_day() {
        // A backlog reaching into yesterday gets a header; today's chat doesn't.
        assert!(is_new_day(None, d(15), d(16)));
        assert!(!is_new_day(None, d(16), d(16)));
    }
}
