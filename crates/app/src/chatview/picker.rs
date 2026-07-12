//! The emote picker: a searchable, virtualized grid of the tab's emotes with
//! per-platform tabs, plus the input-bar button that toggles it. A child module
//! of [`chatview`](super) (it works directly on [`ChatView`]'s picker fields);
//! everything here is picker-only — the chat log never calls in except to
//! render the panel/button and to re-filter when the emote sets change.

use std::collections::HashMap;

use gpui::prelude::*;
use gpui::{
    div, image_cache, img, list, px, App, Context, Entity, FontWeight, MouseButton, SharedString,
    WeakEntity, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{h_flex, v_flex, ActiveTheme};

use super::ChatView;
use crate::image_cache::LruImageCache;
use crate::{PICKER_COLUMNS, PICKER_GRID_HEIGHT};

/// One row in the (virtualized) emote-picker grid: a section header labelling a
/// group of emotes (by provider) or a row of up to
/// [`PICKER_COLUMNS`] emotes belonging to the group above it.
#[derive(PartialEq)]
pub(super) enum PickerRow {
    Header(SharedString),
    Emotes(Vec<bks_core::Emote>),
}

/// Fixed render size of one emote in the picker grid (the `img` height). Held as a
/// const so the cached cell's outer size is stable frame-to-frame (the view cache
/// key includes bounds — a changing size would defeat caching).
const PICKER_EMOTE_PX: f32 = 28.0;

/// One emote in the picker grid, as its **own cached GPUI view**: a cheap
/// **poster** (first-frame) thumbnail with the full **animated** img overlaid.
///
/// The cost model: an animated `img` (one with an element `.id()`) schedules a
/// repaint tick at its GIF's cadence, and gpui dirties a notified view's
/// *ancestors* too — so a grid of animated emotes redraws the window at up to
/// ~50fps while open. That's bounded by keeping the grid short + the overdraw
/// tiny (see `PICKER_GRID_HEIGHT` / the picker `ListState`), by the cached
/// chat-log child view (the heavy subtree reuses its paint), and by pinning
/// full-animation decodes to one background thread while the poster underneath
/// fills the grid instantly. An id-less img (the poster) never ticks at all.
///
/// The caching still matters for everything else: gpui reuses a `.cached()`
/// view's prepaint/paint on any frame where its entity is not dirty, so
/// a scroll/search/one cell's animation tick never rebuilds its siblings. Cells
/// are **persistent** (created once, kept in [`ChatView::picker_cells`] keyed by
/// emote url) and reused across renders — a fresh `cx.new` each frame would
/// always cache-miss.
pub(super) struct EmoteCell {
    name: SharedString,
    url: SharedString,
    /// The owning view, to insert the emote name into its input on click.
    host: WeakEntity<ChatView>,
    /// The app-wide image cache, set on the img directly so the cell renders the
    /// same cached/disk-backed image as chat regardless of ancestor cache scope.
    image_cache: Entity<LruImageCache>,
}

/// Fixed picker-cell box. Width/height are constant so the cached cell's bounds are
/// identical every frame (the view cache key includes bounds; a changing size would
/// force a re-render and defeat the caching). The img scales by height inside, so
/// wide and narrow emotes both fit this uniform grid cell (Discord-style).
const PICKER_CELL_W: f32 = 48.0;
const PICKER_CELL_H: f32 = 38.0;

/// The cached-cell root style: a fixed-size box (see [`PICKER_CELL_W`]/[`PICKER_CELL_H`]).
fn picker_cell_style() -> gpui::StyleRefinement {
    let mut s = gpui::StyleRefinement::default();
    s.size.width = Some(px(PICKER_CELL_W).into());
    s.size.height = Some(px(PICKER_CELL_H).into());
    s
}

impl Render for EmoteCell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host = self.host.clone();
        let name = self.name.clone();
        // Every cell renders a **poster** (`poster://<url>`, first-frame-only
        // decode — see `image_cache`; full-animation decodes were most of the
        // CPU while fast-scrolling) with the real animated img *overlaid* — the
        // poster stays visible underneath while the full decode loads, so
        // scrolling fills instantly and the swap doesn't flicker. The animation's
        // repaint notify lands on *this* cell view (see the type doc), so a
        // frame tick repaints just the cell that advanced.
        let poster =
            SharedString::from(format!("{}{}", crate::image_cache::POSTER_PREFIX, self.url));
        let image = div()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .child(
                img(poster)
                    .image_cache(&self.image_cache)
                    .h(px(PICKER_EMOTE_PX))
                    .max_w(px(PICKER_CELL_W)),
            )
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::animated_img::animated_img(
                            "img",
                            self.url.clone(),
                            px(PICKER_EMOTE_PX),
                        )
                        .max_w(px(PICKER_CELL_W)),
                    ),
            );
        // Fill the fixed cell box (matches `picker_cell_style`) and center the emote.
        div()
            .id("picker-emote")
            .w(px(PICKER_CELL_W))
            .h(px(PICKER_CELL_H))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().secondary))
            .child(image)
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                let name = name.clone();
                let _ = host.update(cx, |this, cx| {
                    this.insert_emote(name.as_ref(), window, cx);
                });
            })
    }
}

impl ChatView {
    /// Toggles the emote picker. On the first open, kicks off the one-time fetch
    /// of the user's personal + viewed-channel Twitch emotes (7TV/etc. already
    /// arrive via the bridge); they merge in when the fetch returns.
    fn toggle_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        if self.picker_open {
            // Show the current set right away (it may have changed since last open).
            self.refresh_picker_filter(cx);
            self.ensure_personal_emotes(cx);
        }
        cx.notify();
    }

    /// Fetches the logged-in user's personal Twitch emotes plus the viewed
    /// channel's native set (sub/follower/bits) once, merging them into
    /// [`personal_emotes`](ChatView::personal_emotes) so both the picker and the
    /// `:`-autocomplete popup see them. Native Twitch emotes bypass the 3rd-party
    /// registry (they come off the IRC tag), so without this they'd only ever
    /// render in chat, never complete. Idempotent via `emotes_fetched`; a no-op
    /// when logged out (the fetch returns empty). Called on first picker open and
    /// on the first `:` typed, whichever comes first.
    pub(super) fn ensure_personal_emotes(&mut self, cx: &mut Context<Self>) {
        if self.emotes_fetched {
            return;
        }
        self.emotes_fetched = true;
        let (tx, rx) = smol::channel::bounded(1);
        self.controller.fetch_twitch_emotes(tx);
        cx.spawn(async move |weak, cx| {
            if let Ok(emotes) = rx.recv().await {
                let _ = weak.update(cx, |this, cx| {
                    this.personal_emotes = emotes;
                    if this.picker_open {
                        this.refresh_picker_filter(cx);
                    }
                    // If a `:`-emote popup is already open (the fetch was triggered
                    // by typing `:`), recompute it now that the emotes landed —
                    // otherwise it'd stay empty until the next keystroke.
                    this.refresh_emote_popup(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The current emote-picker search text (lowercased), or empty for "show all".
    fn picker_query(&self, cx: &App) -> String {
        self.picker_search.read(cx).value().trim().to_lowercase()
    }

    /// The emotes for the picker's currently selected platform tab: the channel
    /// emotes (from the shared model) followed (on the Twitch tab) by the user's
    /// personal Twitch emotes. Cloned out (cheap `Arc` string data) so no model
    /// borrow is held.
    fn picker_tab_emotes(&self, cx: &App) -> Vec<bks_core::Emote> {
        let model = self.channel.read(cx);
        match self.picker_tab {
            bks_core::Platform::Kick => model.emotes_kick.clone(),
            bks_core::Platform::YouTube => model.emotes_youtube.clone(),
            _ => model
                .emotes_twitch
                .iter()
                .chain(self.personal_emotes.iter())
                .cloned()
                .collect(),
        }
    }

    /// Which platform tabs the picker shows: only the platforms this tab has a
    /// channel for (so a Twitch-only tab shows no Kick tab, and vice versa).
    fn picker_platforms(&self) -> Vec<bks_core::Platform> {
        let mut out = Vec::new();
        if !self.config.twitch_channel.is_empty() {
            out.push(bks_core::Platform::Twitch);
        }
        if !self.config.kick_channel.is_empty() {
            out.push(bks_core::Platform::Kick);
        }
        if !self.config.youtube_channel.is_empty() {
            out.push(bks_core::Platform::YouTube);
        }
        out
    }

    /// Switches the picker to `platform`'s emotes, resetting the search and grid.
    fn set_picker_tab(&mut self, platform: bks_core::Platform, cx: &mut Context<Self>) {
        if self.picker_tab == platform {
            return;
        }
        self.picker_tab = platform;
        self.refresh_picker_filter(cx);
    }

    /// Recomputes the picker's display rows for the active platform tab: the tab's
    /// emotes filtered by the search box, grouped by provider into labelled
    /// sections, each section a header row followed by rows of
    /// [`PICKER_COLUMNS`] emotes. Groups keep the order their providers first appear
    /// (channel emotes lead). Re-measures the virtualized list to match.
    pub(super) fn refresh_picker_filter(&mut self, cx: &mut Context<Self>) {
        let query = self.picker_query(cx);
        // Filter, preserving order; group by provider with first-seen ordering.
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<bks_core::Emote>> = HashMap::new();
        for emote in self.picker_tab_emotes(cx) {
            if !query.is_empty() && !emote.name.to_lowercase().contains(&query) {
                continue;
            }
            let provider = if emote.tooltip.provider.is_empty() {
                "Emotes".to_string()
            } else {
                emote.tooltip.provider.clone()
            };
            if !groups.contains_key(&provider) {
                order.push(provider.clone());
            }
            groups.entry(provider).or_default().push(emote);
        }

        let mut rows: Vec<PickerRow> = Vec::new();
        for provider in order {
            let emotes = groups.remove(&provider).unwrap_or_default();
            if emotes.is_empty() {
                continue;
            }
            rows.push(PickerRow::Header(SharedString::from(provider)));
            for chunk in emotes.chunks(PICKER_COLUMNS) {
                rows.push(PickerRow::Emotes(chunk.to_vec()));
            }
        }

        // Unchanged grid — bail before the `reset` below, which snaps scroll back
        // to the top. This runs on *every* channel event while the picker is open
        // (a new chat message re-triggers it via `on_channel_event`), so without
        // this the grid jumped to the top whenever anyone chatted.
        if rows == self.picker_rows {
            return;
        }

        // Rebuild the persistent cell views to exactly the filtered set: drop any no
        // longer shown, then create one per emote we don't already have. Reusing the
        // same `Entity<EmoteCell>` across renders is what lets gpui cache + skip a
        // cell whose emote didn't animate this frame (a fresh view would cache-miss).
        let shown: Vec<(SharedString, SharedString)> = rows
            .iter()
            .filter_map(|row| match row {
                PickerRow::Emotes(emotes) => Some(emotes),
                _ => None,
            })
            .flatten()
            .map(|e| {
                (
                    SharedString::from(e.url.clone()),
                    SharedString::from(e.name.clone()),
                )
            })
            .collect();
        // Set-based retain: this runs per keystroke (and on every emote-set
        // update), and a linear `shown.iter().any()` inside `retain` was
        // O(cells × shown) — millions of string compares for a big 7TV set.
        let shown_urls: std::collections::HashSet<&SharedString> =
            shown.iter().map(|(url, _)| url).collect();
        self.picker_cells.retain(|url, _| shown_urls.contains(url));
        drop(shown_urls);
        let host = cx.entity().downgrade();
        for (url, name) in shown {
            // Not `entry().or_insert_with`: building the cell needs `cx.new`, which
            // can't borrow `cx` while the entry holds `&mut self.picker_cells`.
            #[allow(clippy::map_entry)]
            if !self.picker_cells.contains_key(&url) {
                let host = host.clone();
                let image_cache = self.image_cache.clone();
                let cell = cx.new(|_| EmoteCell {
                    name,
                    url: url.clone(),
                    host,
                    image_cache,
                });
                self.picker_cells.insert(url, cell);
            }
        }

        self.picker_rows = rows;
        self.picker_list_state.reset(self.picker_rows.len());
        cx.notify();
    }

    /// Re-filters the grid when the search box changes.
    pub(super) fn on_picker_search_event(
        &mut self,
        _state: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::Change = event {
            self.refresh_picker_filter(cx);
        }
    }

    /// Inserts an emote's name into the input (with a trailing space) at the end
    /// of the current text, so a click appends it to the message being typed.
    fn insert_emote(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.input.read(cx).value().to_string();
        // Separate from preceding text with a space unless the box is empty or
        // already ends in whitespace.
        let sep = if current.is_empty() || current.ends_with(char::is_whitespace) {
            ""
        } else {
            " "
        };
        let next = format!("{current}{sep}{name} ");
        self.input.update(cx, |state, cx| {
            state.set_value(&next, window, cx);
        });
        self.completion = None;
    }

    /// One platform tab chip in the emote picker. Highlighted when it's the active
    /// tab; clicking it switches the grid to that platform's emotes.
    fn picker_tab_chip(
        &self,
        platform: bks_core::Platform,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let selected = self.picker_tab == platform;
        let color = platform.color().to_u32();
        div()
            .id(SharedString::from(format!(
                "picker-tab-{}",
                platform.label()
            )))
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .font_weight(FontWeight::MEDIUM)
            .when(selected, |s| {
                s.bg(cx.theme().secondary).text_color(gpui::rgb(color))
            })
            .when(!selected, |s| s.text_color(cx.theme().muted_foreground))
            .hover(|s| s.bg(cx.theme().secondary))
            .child(SharedString::from(platform.label()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.set_picker_tab(platform, cx)),
            )
            .into_any_element()
    }

    /// The emote-picker panel shown above the input bar when open: platform tabs
    /// (Twitch/Kick — only those the tab has a channel for) over a search box and a
    /// *virtualized* grid of emote images (only on-screen rows are built, so a
    /// large 7TV set stays light), each a click target that inserts its name. The
    /// search box filters the active tab's emotes by name. Rows are fixed-width
    /// chunks of [`PICKER_COLUMNS`] emotes so the grid can be windowed with
    /// `gpui::list`.
    pub(super) fn render_emote_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let platforms = self.picker_platforms();
        // Only show the tab strip when the tab has more than one platform (a
        // single-platform tab needs no switcher).
        let tabs = (platforms.len() > 1).then(|| {
            h_flex()
                .w_full()
                .gap_1()
                .px_1()
                .children(platforms.iter().map(|p| self.picker_tab_chip(*p, cx)))
        });

        let search = h_flex()
            .w_full()
            .px_1()
            .child(div().flex_1().child(Input::new(&self.picker_search)));

        let body = if self.picker_rows.is_empty() {
            let msg = if !self.picker_query(cx).is_empty() {
                "No emotes match your search.".to_string()
            } else if self.picker_tab == bks_core::Platform::Kick {
                "No Kick emotes yet (loads on connect).".to_string()
            } else {
                "No emotes yet (channel emotes load on connect; log into Twitch for your own)."
                    .to_string()
            };
            div()
                .p_2()
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(msg))
                .into_any_element()
        } else {
            // The grid is windowed: one list item per row (a section header or a
            // row of emotes), so only on-screen rows are built. The row closure
            // runs with a bare `&mut App`, so a cell's click goes through the view
            // entity (like the chat log's name clicks) rather than `cx.listener`.
            let entity = cx.entity();
            div()
                .w_full()
                .h(px(PICKER_GRID_HEIGHT))
                // Route the picker's emote images through the same app-wide cache as
                // the chat log, so emotes already seen in chat appear instantly (and
                // benefit from the disk cache) instead of re-fetching via gpui's
                // separate global cache.
                .child(
                    image_cache(self.image_cache.clone()).size_full().child(
                        list(
                            self.picker_list_state.clone(),
                            move |row_ix, _window, cx| {
                                let this = entity.read(cx);
                                match this.picker_rows.get(row_ix) {
                                    Some(PickerRow::Header(label)) => div()
                                        .w_full()
                                        .px_1()
                                        .pt_2()
                                        .pb_1()
                                        .text_size(px(11.))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(
                                            gpui_component::ActiveTheme::theme(cx).muted_foreground,
                                        )
                                        .child(label.clone())
                                        .into_any_element(),
                                    Some(PickerRow::Emotes(emotes)) => {
                                        // Each emote renders through its persistent cached
                                        // cell view (built in `refresh_picker_filter`), so an
                                        // animation tick repaints only the cell that advanced.
                                        // The `.cached(..)` style fixes the cell's box size so
                                        // the view cache key's bounds stay stable frame-to-frame.
                                        let cells = emotes.iter().filter_map(|emote| {
                                            let url = SharedString::from(emote.url.clone());
                                            this.picker_cells.get(&url).map(|cell| {
                                                gpui::AnyView::from(cell.clone())
                                                    .cached(picker_cell_style())
                                                    .into_any_element()
                                            })
                                        });
                                        h_flex().gap_1().children(cells).into_any_element()
                                    }
                                    None => div().into_any_element(),
                                }
                            },
                        )
                        .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
                        .size_full(),
                    ),
                )
                .into_any_element()
        };

        v_flex()
            .id("emote-picker")
            .w_full()
            .p_2()
            .gap_1()
            .bg(cx.theme().background)
            .border_t_1()
            .border_color(cx.theme().border)
            .children(tabs)
            .child(search)
            .child(body)
            .into_any_element()
    }

    /// The input-bar button that opens/closes the emote picker.
    pub(super) fn picker_button(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Compact — it sits *inside* the input box as its suffix. The negative
        // margin pulls it toward the box's right edge: the kit re-asserts its
        // 12px padding when a suffix is set, which read as a stray gap.
        div()
            .id("emote-picker-toggle")
            .px_1p5()
            .mr(px(-6.))
            .rounded_sm()
            .cursor_pointer()
            .text_color(cx.theme().muted_foreground)
            .hover(|s| {
                s.bg(crate::render::chrome_hover())
                    .text_color(cx.theme().foreground)
            })
            .child(SharedString::from("☺"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.toggle_picker(cx)),
            )
            .into_any_element()
    }
}
