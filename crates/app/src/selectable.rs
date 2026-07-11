//! Drag-to-select text for the chat log (Route B: a custom element over gpui's
//! text layout).
//!
//! Chat rows are a flex-wrap of word/emote tokens, so no single gpui text layout
//! spans a message. Instead each contiguous *text* run is one [`SelectableText`]
//! element backed by a [`StyledText`]; emote/badge images stay as their own
//! `img()` elements (preserving sizing + animation ids). A shared [`Selection`]
//! coordinates them: every token gets a document-order `ordinal`, registers its
//! laid-out [`TextLayout`] into the selection each frame, and—when the active
//! selection covers part of its text—paints a highlight behind that sub-range.
//!
//! The owning view drives selection with three mouse gestures (down/move/up)
//! that hit-test against the registry, and a copy action that walks the
//! registered tokens in `ordinal` order. The registry is rebuilt every paint, so
//! it always reflects what is currently on screen (rows scrolled out of the ring
//! buffer simply drop out, truncating the selection harmlessly).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::Range;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    fill, px, quad, App, BorderStyle, Bounds, ElementId, GlobalElementId, Hitbox, HitboxBehavior,
    Hsla, InspectorElementId, LayoutId, Pixels, Point, SharedString, StyledText, TextLayout,
    Window,
};

/// Highlight color painted behind selected text (a translucent blue).
fn selection_color() -> Hsla {
    Hsla {
        h: 0.6,
        s: 0.8,
        l: 0.5,
        a: 0.35,
    }
}

/// Solid border drawn around a selected emote. An opaque emote covers the tint
/// fill, so a bright frame is what actually reads as "selected".
fn selection_border() -> Hsla {
    Hsla {
        h: 0.6,
        s: 0.9,
        l: 0.65,
        a: 0.95,
    }
}

/// One laid-out text token, recorded during paint so the owning view can
/// hit-test mouse positions and read back selected text.
struct TokenInfo {
    kind: TokenKind,
    /// Copy text: the run's text for a text token, the emote name for an image.
    text: SharedString,
    /// True for the first token of a message, so copied text gets a newline
    /// between messages (the name token starts each row).
    starts_row: bool,
}

/// How a token is laid out — drives hit-testing and highlight painting. Text
/// tokens hit-test per byte via their layout; image tokens (emotes) are
/// all-or-nothing over their bounds.
enum TokenKind {
    Text(TextLayout),
    /// An emote image: its on-screen bounds. Selecting it copies its name whole.
    Image(Bounds<Pixels>),
}

impl TokenInfo {
    fn bounds(&self) -> Bounds<Pixels> {
        match &self.kind {
            TokenKind::Text(layout) => layout.bounds(),
            TokenKind::Image(bounds) => *bounds,
        }
    }

    /// The byte offset within this token nearest to `point`. Text tokens defer to
    /// their layout; image tokens snap to 0 (left of center) or full length.
    fn index_for_position(&self, point: Point<Pixels>) -> usize {
        match &self.kind {
            TokenKind::Text(layout) => match layout.index_for_position(point) {
                Ok(i) | Err(i) => i,
            },
            TokenKind::Image(bounds) => {
                if point.x < bounds.center().x {
                    0
                } else {
                    self.text.len()
                }
            }
        }
    }
}

/// A selection endpoint: a token's document-order `ordinal` plus a byte offset
/// into that token's text. Ordered first by ordinal, then by byte, so comparing
/// two endpoints gives their document order directly.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Endpoint {
    ordinal: usize,
    byte: usize,
}

/// Shared selection state for one chat log. Cloned into every [`SelectableText`]
/// token (cheap `Rc`) and held by the view.
#[derive(Clone, Default)]
pub struct Selection(Rc<RefCell<SelectionInner>>);

#[derive(Default)]
struct SelectionInner {
    /// Where the drag started, and where it currently is. `None` until a drag.
    anchor: Option<Endpoint>,
    cursor: Option<Endpoint>,
    /// True between mouse-down and mouse-up.
    selecting: bool,
    /// Tokens laid out this frame, keyed by ordinal. Rebuilt every paint.
    tokens: BTreeMap<usize, TokenInfo>,
    /// The link currently under the cursor (its shared id), so every piece of a
    /// wrapped link underlines together. `None` when no link is hovered.
    hovered_link: Option<u64>,
}

impl Selection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears the per-frame token registry. The view calls this at the start of
    /// each render so stale (scrolled-away) tokens don't linger.
    pub fn begin_frame(&self) {
        self.0.borrow_mut().tokens.clear();
    }

    /// Records a text token's laid-out layout for this frame.
    fn register_text(
        &self,
        ordinal: usize,
        layout: TextLayout,
        text: SharedString,
        starts_row: bool,
    ) {
        self.register(ordinal, TokenKind::Text(layout), text, starts_row);
    }

    /// Records an image token (emote) and its bounds for this frame; selecting it
    /// contributes `text` (the emote name) to a copy.
    fn register_image(&self, ordinal: usize, bounds: Bounds<Pixels>, text: SharedString) {
        self.register(ordinal, TokenKind::Image(bounds), text, false);
    }

    fn register(&self, ordinal: usize, kind: TokenKind, text: SharedString, starts_row: bool) {
        self.0.borrow_mut().tokens.insert(
            ordinal,
            TokenInfo {
                kind,
                text,
                starts_row,
            },
        );
    }

    /// Whether there is a non-empty selection.
    pub fn has_selection(&self) -> bool {
        let inner = self.0.borrow();
        matches!((inner.anchor, inner.cursor), (Some(a), Some(c)) if a != c)
    }

    /// Sets (or clears) the hovered link id, returning whether it changed (so the
    /// view only repaints on a real change). All pieces of one wrapped link share
    /// an id, so hovering any piece highlights the whole link.
    pub fn set_hovered_link(&self, link: Option<u64>) -> bool {
        let mut inner = self.0.borrow_mut();
        if inner.hovered_link == link {
            false
        } else {
            inner.hovered_link = link;
            true
        }
    }

    /// Whether the link `id` is the one currently hovered.
    pub fn is_link_hovered(&self, id: u64) -> bool {
        self.0.borrow().hovered_link == Some(id)
    }

    /// The selection's `(start, end)` endpoints in document order, or `None` if
    /// there is no (non-empty) selection.
    fn ordered(inner: &SelectionInner) -> Option<(Endpoint, Endpoint)> {
        let (a, c) = (inner.anchor?, inner.cursor?);
        if a == c {
            return None;
        }
        Some(if a < c { (a, c) } else { (c, a) })
    }

    /// The selected byte range within the token at `ordinal`, if any part of it
    /// is selected. Used by that token to paint its highlight.
    fn range_for(&self, ordinal: usize) -> Option<Range<usize>> {
        let inner = self.0.borrow();
        let (start, end) = Self::ordered(&inner)?;
        let len = inner.tokens.get(&ordinal)?.text.len();
        let (from, to) = token_span(ordinal, start, end, len)?;
        (from < to).then_some(from..to)
    }

    /// Whether the token at `ordinal` is (wholly or partly) within the selection.
    /// Used by image tokens, which highlight as a whole rather than per byte.
    fn is_selected(&self, ordinal: usize) -> bool {
        let inner = self.0.borrow();
        let Some((start, end)) = Self::ordered(&inner) else {
            return false;
        };
        // Any non-empty span means the (all-or-nothing) image is selected.
        token_span(ordinal, start, end, self.text_len(&inner, ordinal))
            .is_some_and(|(from, to)| from < to)
    }

    fn text_len(&self, inner: &SelectionInner, ordinal: usize) -> usize {
        inner.tokens.get(&ordinal).map_or(0, |t| t.text.len())
    }

    /// Hit-tests a window point to the nearest endpoint. Prefers a token whose
    /// bounds contain the point; otherwise picks the closest. `None` if no tokens
    /// are laid out.
    fn hit(&self, point: Point<Pixels>) -> Option<Endpoint> {
        let inner = self.0.borrow();
        inner
            .tokens
            .iter()
            .map(|(&ordinal, info)| {
                let endpoint = Endpoint {
                    ordinal,
                    byte: info.index_for_position(point),
                };
                (endpoint, distance_to_bounds(info.bounds(), point))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(endpoint, _)| endpoint)
    }

    /// Begins a selection at `point` (mouse down). Clears any prior selection.
    pub fn start(&self, point: Point<Pixels>) {
        let endpoint = self.hit(point);
        let mut inner = self.0.borrow_mut();
        inner.anchor = endpoint;
        inner.cursor = endpoint;
        inner.selecting = true;
    }

    /// Extends the selection to `point` (mouse move while dragging). Returns
    /// whether the cursor moved (so the view can avoid redundant repaints).
    pub fn extend(&self, point: Point<Pixels>) -> bool {
        if !self.0.borrow().selecting {
            return false;
        }
        let endpoint = self.hit(point);
        let mut inner = self.0.borrow_mut();
        if inner.cursor != endpoint {
            inner.cursor = endpoint;
            true
        } else {
            false
        }
    }

    /// Ends the drag (mouse up). Keeps the selection so it can be copied.
    pub fn finish(&self) {
        self.0.borrow_mut().selecting = false;
    }

    pub fn is_selecting(&self) -> bool {
        self.0.borrow().selecting
    }

    /// Assembles the selected text in document order: the first and last tokens
    /// are sliced to their selected byte ranges, middle tokens taken whole, with
    /// a newline inserted between messages (at each row-starting token).
    pub fn selected_text(&self) -> String {
        let inner = self.0.borrow();
        let Some((start, end)) = Self::ordered(&inner) else {
            return String::new();
        };
        let tokens = inner
            .tokens
            .range(start.ordinal..=end.ordinal)
            .map(|(&ord, info)| (ord, info.text.as_ref(), info.starts_row));
        assemble_selection(tokens, start, end)
    }
}

/// The selected byte sub-range `[from, to)` within token `ordinal`, clamped to
/// `len`, given the selection's ordered endpoints — or `None` if the token lies
/// outside the selection. The single source of truth for which bytes of a token
/// are selected (highlighting, copy, and image hit-testing all use it).
fn token_span(
    ordinal: usize,
    start: Endpoint,
    end: Endpoint,
    len: usize,
) -> Option<(usize, usize)> {
    if ordinal < start.ordinal || ordinal > end.ordinal {
        return None;
    }
    let from = if ordinal == start.ordinal {
        start.byte
    } else {
        0
    };
    let to = if ordinal == end.ordinal {
        end.byte
    } else {
        len
    };
    Some((from.min(len), to.min(len)))
}

/// Pure copy-assembly: given selected tokens in ordinal order (with each token's
/// text and whether it starts a new message), slice each to its selected bytes
/// and join messages with newlines. Separated out so it can be unit-tested
/// without a GPU layout.
fn assemble_selection<'a>(
    tokens: impl Iterator<Item = (usize, &'a str, bool)>,
    start: Endpoint,
    end: Endpoint,
) -> String {
    let mut out = String::new();
    let mut first = true;
    for (ordinal, text, starts_row) in tokens {
        if starts_row && !first {
            out.push('\n');
        }
        first = false;
        if let Some((from, to)) = token_span(ordinal, start, end, text.len()) {
            out.push_str(&text[from..to]);
        }
    }
    out
}

/// Manhattan-ish distance from a point to the nearest edge of `bounds` (0 inside).
fn distance_to_bounds(bounds: Bounds<Pixels>, point: Point<Pixels>) -> f32 {
    let dx = (bounds.left() - point.x)
        .max(point.x - bounds.right())
        .max(px(0.));
    let dy = (bounds.top() - point.y)
        .max(point.y - bounds.bottom())
        .max(px(0.));
    f32::from(dx) + f32::from(dy)
}

/// A selectable run of text. Wraps a [`StyledText`]; on paint it registers its
/// layout into the shared [`Selection`] and draws a highlight behind any
/// selected sub-range before painting the text.
pub struct SelectableText {
    id: ElementId,
    ordinal: usize,
    text: SharedString,
    /// True for a message's first token, so copy inserts a newline before it.
    starts_row: bool,
    styled: StyledText,
    selection: Selection,
}

impl SelectableText {
    pub fn new(
        id: impl Into<ElementId>,
        ordinal: usize,
        text: impl Into<SharedString>,
        selection: Selection,
    ) -> Self {
        let text = text.into();
        Self {
            id: id.into(),
            ordinal,
            text: text.clone(),
            starts_row: false,
            styled: StyledText::new(text),
            selection,
        }
    }

    /// Marks this token as the start of a message (the author-name token), so
    /// copied multi-message selections are newline-separated.
    pub fn starts_row(mut self, starts_row: bool) -> Self {
        self.starts_row = starts_row;
        self
    }
}

impl IntoElement for SelectableText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (layout_id, ()) = self.styled.request_layout(None, inspector_id, window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.styled
            .prepaint(None, inspector_id, bounds, state, window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        _hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        // Register this token for hit-testing + copy before painting.
        self.selection.register_text(
            self.ordinal,
            layout.clone(),
            self.text.clone(),
            self.starts_row,
        );

        // Paint the highlight behind the selected sub-range, if any.
        if let Some(range) = self.selection.range_for(self.ordinal) {
            paint_highlight(&layout, &self.text, range, window);
        }

        self.styled
            .paint(None, inspector_id, bounds, state, &mut (), window, cx);
    }
}

/// An emote image that participates in text selection: it registers its bounds
/// (under its document-order `ordinal`) so a drag crossing it includes the emote
/// and a copy emits its name. Highlights as a whole when selected. Wraps the
/// already-configured `img()` element so sizing/animation ids are unchanged.
pub struct SelectableImage {
    id: ElementId,
    ordinal: usize,
    name: SharedString,
    selection: Selection,
    child: gpui::AnyElement,
}

impl SelectableImage {
    pub fn new(
        id: impl Into<ElementId>,
        ordinal: usize,
        name: impl Into<SharedString>,
        selection: Selection,
        child: impl IntoElement,
    ) -> Self {
        Self {
            id: id.into(),
            ordinal,
            name: name.into(),
            selection,
            child: child.into_any_element(),
        }
    }
}

impl IntoElement for SelectableImage {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for SelectableImage {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        // Register with the laid-out bounds so hit-testing snaps to this emote.
        self.selection
            .register_image(self.ordinal, bounds, self.name.clone());
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut (),
        _prepaint: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let selected = self.selection.is_selected(self.ordinal);
        let framed = if selected {
            // Inflate the bounds a touch so a blue frame + margin shows around
            // the emote (drawn behind it, then a wash drawn over it below).
            let pad = px(2.);
            let framed = Bounds {
                origin: gpui::point(bounds.origin.x - pad, bounds.origin.y - pad),
                size: gpui::size(bounds.size.width + pad * 2., bounds.size.height + pad * 2.),
            };
            window.paint_quad(quad(
                framed,
                px(3.),
                selection_color(),
                px(1.5),
                selection_border(),
                BorderStyle::Solid,
            ));
            Some(framed)
        } else {
            None
        };

        self.child.paint(window, cx);

        // Paint the tint a second time *over* the (opaque) emote so the whole
        // emote body reads as selected, not just the frame around it.
        if let Some(framed) = framed {
            window.paint_quad(fill(framed, selection_color()).corner_radii(px(3.)));
        }
    }
}

/// Paints selection highlight quads over the byte `range` of a laid-out token,
/// one quad per line the range spans (a wrapped token can cover several lines).
fn paint_highlight(layout: &TextLayout, text: &str, range: Range<usize>, window: &mut Window) {
    let line_height = layout.line_height();
    let color = selection_color();

    // Walk the range char-boundary by char-boundary, grouping consecutive
    // positions that share a line into one quad. position_for_index gives the
    // top-left of each index; the gap to the next index is the glyph advance.
    let mut idx = range.start;
    while idx < range.end {
        let Some(start_pos) = layout.position_for_index(idx) else {
            break;
        };
        // Extend along this line until the y changes (a wrap) or range ends.
        let mut end = next_boundary(text, idx);
        let mut end_pos = layout.position_for_index(end).unwrap_or(start_pos);
        while end < range.end {
            let next = next_boundary(text, end);
            let Some(p) = layout.position_for_index(next) else {
                break;
            };
            if p.y != start_pos.y {
                break; // wrapped to a new line; close this quad here.
            }
            end = next;
            end_pos = p;
        }
        let quad_bounds = Bounds {
            origin: start_pos,
            size: gpui::size(end_pos.x - start_pos.x, line_height),
        };
        if quad_bounds.size.width > px(0.) {
            window.paint_quad(fill(quad_bounds, color));
        }
        idx = end;
    }
}

/// Next UTF-8 char boundary after `idx` in `text` (clamped to len).
fn next_boundary(text: &str, idx: usize) -> usize {
    let mut next = idx + 1;
    while next < text.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    next.min(text.len())
}

#[cfg(test)]
mod tests {
    use super::{assemble_selection, Endpoint};

    fn at(ordinal: usize, byte: usize) -> Endpoint {
        Endpoint { ordinal, byte }
    }

    /// Assembles a selection over `(ordinal, text, starts_row)` tokens.
    fn select(tokens: &[(usize, &'static str, bool)], start: Endpoint, end: Endpoint) -> String {
        assemble_selection(tokens.iter().copied(), start, end)
    }

    #[test]
    fn empty_when_endpoints_equal() {
        assert_eq!(select(&[(0, "hello", true)], at(0, 2), at(0, 2)), "");
    }

    #[test]
    fn single_token_partial() {
        assert_eq!(select(&[(0, "hello", true)], at(0, 1), at(0, 4)), "ell");
    }

    #[test]
    fn across_tokens_slices_ends_takes_middle_whole() {
        let t = [
            (0, "name:", true),
            (1, " hello ", false),
            (2, "world", false),
        ];
        assert_eq!(select(&t, at(0, 2), at(2, 3)), "me: hello wor");
    }

    #[test]
    fn newline_between_messages() {
        let t = [
            (0, "alice:", true),
            (1, " hi", false),
            (2, "bob:", true),
            (3, " yo", false),
        ];
        assert_eq!(select(&t, at(0, 0), at(3, 3)), "alice: hi\nbob: yo");
    }

    #[test]
    fn no_leading_newline_when_first_token_starts_row() {
        let t = [(0, "alice:", true), (1, " hi", false)];
        assert_eq!(select(&t, at(0, 0), at(1, 3)), "alice: hi");
    }

    #[test]
    fn byte_endpoints_clamped_to_token_len() {
        assert_eq!(select(&[(0, "hi", true)], at(0, 0), at(0, 99)), "hi");
    }

    #[test]
    fn emote_in_middle_copies_its_name() {
        // Text "say " | emote "Kappa" (offsets 0/len) | text " end".
        let t = [(0, "say ", true), (1, "Kappa", false), (2, " end", false)];
        assert_eq!(select(&t, at(0, 0), at(2, 4)), "say Kappa end");
    }

    #[test]
    fn emote_excluded_when_selection_ends_at_its_left_edge() {
        let t = [(0, "say ", true), (1, "Kappa", false)];
        assert_eq!(select(&t, at(0, 0), at(1, 0)), "say ");
    }
}
