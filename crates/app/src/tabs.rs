//! Persisted tab list: each tab's name + its Twitch/Kick channels.
//!
//! Saved to `<config>/backseater/tabs.json` and restored on launch. A tab may
//! have either channel blank (it then connects only the filled platform).

use bks_platform::EventKind;
use serde::{Deserialize, Serialize};

const STORE_NAME: &str = "tabs";
/// Separate store for the index of the last-active tab, so reopening the app
/// returns to the tab the user was on. Kept apart from the tab list so its
/// (frequent) updates don't rewrite the whole list, and so old installs without
/// it just default to the first tab.
const ACTIVE_STORE_NAME: &str = "active_tab";

/// Generates a one-bool-per-[`EventKind`] toggle struct — the shared shape of
/// the events panel's visibility filter and the alert-sound picks, so adding a
/// kind is one edit here, not parallel match arms per struct. Each is stored as
/// named bools so old `tabs.json` files (missing a field or the whole struct)
/// fill from `$default` via serde's container `default`, and serialization is
/// by kind name, not position, so reordering the [`EventKind`] enum can't
/// silently flip the wrong toggles. (A single const-generic struct would be
/// tidier still, but serde's derive doesn't support const generics.)
macro_rules! event_toggles {
    ($(#[$doc:meta])* $name:ident, default = $default:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
        #[serde(default)]
        pub struct $name {
            pub sub: bool,
            pub gift: bool,
            pub raid: bool,
            pub bits: bool,
            pub reward: bool,
            pub watch_streak: bool,
            pub announcement: bool,
            pub other: bool,
        }

        impl Default for $name {
            fn default() -> Self {
                Self {
                    sub: $default,
                    gift: $default,
                    raid: $default,
                    bits: $default,
                    reward: $default,
                    watch_streak: $default,
                    announcement: $default,
                    other: $default,
                }
            }
        }

        impl $name {
            /// Whether `kind`'s toggle is on.
            pub fn enabled(&self, kind: EventKind) -> bool {
                match kind {
                    EventKind::Sub => self.sub,
                    EventKind::Gift => self.gift,
                    EventKind::Raid => self.raid,
                    EventKind::Bits => self.bits,
                    EventKind::Reward => self.reward,
                    EventKind::WatchStreak => self.watch_streak,
                    EventKind::Announcement => self.announcement,
                    EventKind::Other => self.other,
                }
            }

            /// Mutable access to the toggle for `kind`, for the settings UI.
            pub fn toggle_mut(&mut self, kind: EventKind) -> &mut bool {
                match kind {
                    EventKind::Sub => &mut self.sub,
                    EventKind::Gift => &mut self.gift,
                    EventKind::Raid => &mut self.raid,
                    EventKind::Bits => &mut self.bits,
                    EventKind::Reward => &mut self.reward,
                    EventKind::WatchStreak => &mut self.watch_streak,
                    EventKind::Announcement => &mut self.announcement,
                    EventKind::Other => &mut self.other,
                }
            }
        }
    };
}

event_toggles!(
    /// Which event kinds the events panel shows (all on by default).
    EventFilter,
    default = true
);
event_toggles!(
    /// Which event kinds play the alert ping when they arrive live. All off by
    /// default — sounds are opt-in, like the mention ping — and independent of
    /// [`EventFilter`]: a kind can ping without showing in the panel and vice
    /// versa.
    EventSounds,
    default = false
);

/// What one cell of a tab's layout grid shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PanelKind {
    Chat,
    Events,
    Mentions,
}

impl PanelKind {
    /// The other auxiliary panel (used to co-locate a newly enabled panel with
    /// its sibling). Chat has no counterpart.
    fn sibling(self) -> Option<PanelKind> {
        match self {
            PanelKind::Chat => None,
            PanelKind::Events => Some(PanelKind::Mentions),
            PanelKind::Mentions => Some(PanelKind::Events),
        }
    }
}

/// One panel in a layout column. `share` is its height fraction within the
/// column (normalized so a column's panels sum to 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutPanel {
    pub kind: PanelKind,
    #[serde(default = "one")]
    pub share: f32,
}

/// One column of the layout grid. `share` is its width fraction (normalized so
/// all columns sum to 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutColumn {
    pub panels: Vec<LayoutPanel>,
    #[serde(default = "one")]
    pub share: f32,
}

fn one() -> f32 {
    1.0
}

/// Where a panel header's arrow button moves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveDir {
    Left,
    Right,
    Up,
    Down,
}

/// The smallest share a column/panel can be dragged or normalized to, so
/// nothing collapses to invisible.
pub const MIN_SHARE: f32 = 0.10;

/// A tab's panel arrangement: columns left-to-right, each stacking panels
/// top-to-bottom, sized by fractional shares (resizable by dragging the
/// dividers between them). Chat is just another panel, so any arrangement of
/// chat/events/mentions is expressible; every kind appears at most once and
/// chat always exists ([`sanitize`](Self::sanitize) enforces both).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Layout {
    pub columns: Vec<LayoutColumn>,
}

impl Layout {
    /// The minimal layout: one column holding just the chat.
    pub fn single_chat() -> Self {
        Self {
            columns: vec![LayoutColumn {
                panels: vec![LayoutPanel {
                    kind: PanelKind::Chat,
                    share: 1.0,
                }],
                share: 1.0,
            }],
        }
    }

    /// Whether the layout shows `kind`.
    pub fn contains(&self, kind: PanelKind) -> bool {
        self.position(kind).is_some()
    }

    /// The `(column, row)` of `kind`, if present.
    pub fn position(&self, kind: PanelKind) -> Option<(usize, usize)> {
        self.columns.iter().enumerate().find_map(|(ci, col)| {
            col.panels
                .iter()
                .position(|p| p.kind == kind)
                .map(|ri| (ci, ri))
        })
    }

    /// Shows/hides an auxiliary panel. A newly enabled one joins its sibling
    /// panel's column (stacking, like the old side column) or opens a new
    /// rightmost column at a quarter of the width. Chat can't be hidden.
    pub fn set_enabled(&mut self, kind: PanelKind, on: bool) {
        if kind == PanelKind::Chat {
            return;
        }
        match (on, self.position(kind)) {
            (true, None) => {
                if let Some((ci, _)) = kind.sibling().and_then(|s| self.position(s)) {
                    let n = self.columns[ci].panels.len() as f32;
                    self.columns[ci].panels.push(LayoutPanel {
                        kind,
                        share: 1.0 / (n + 1.0),
                    });
                } else {
                    for col in &mut self.columns {
                        col.share *= 0.75;
                    }
                    self.columns.push(LayoutColumn {
                        panels: vec![LayoutPanel { kind, share: 1.0 }],
                        share: 0.25,
                    });
                }
            }
            (false, Some((ci, ri))) => {
                self.columns[ci].panels.remove(ri);
            }
            _ => {}
        }
        self.sanitize();
    }

    /// Moves `kind` one step. Left/Right join the adjacent column (keeping
    /// roughly the same row), or split off a new edge column when there is no
    /// neighbor — so pressing ◀ twice from a shared column first stacks into
    /// the next column, then breaks out into its own. Up/Down swap within the
    /// column. Impossible moves (alone at an edge) are no-ops.
    pub fn move_panel(&mut self, kind: PanelKind, dir: MoveDir) {
        let Some((ci, ri)) = self.position(kind) else {
            return;
        };
        match dir {
            MoveDir::Up if ri > 0 => self.columns[ci].panels.swap(ri, ri - 1),
            MoveDir::Down if ri + 1 < self.columns[ci].panels.len() => {
                self.columns[ci].panels.swap(ri, ri + 1)
            }
            MoveDir::Left | MoveDir::Right => {
                let last = self.columns.len() - 1;
                let dest = match dir {
                    MoveDir::Left if ci > 0 => Some(ci - 1),
                    MoveDir::Right if ci < last => Some(ci + 1),
                    _ => None,
                };
                let at_edge = dest.is_none();
                if at_edge && self.columns[ci].panels.len() <= 1 {
                    return; // Alone at the edge: nowhere to go.
                }
                let panel = self.columns[ci].panels.remove(ri);
                match dest {
                    Some(di) => {
                        let col = &mut self.columns[di];
                        let at = ri.min(col.panels.len());
                        let share = 1.0 / (col.panels.len() as f32 + 1.0);
                        col.panels.insert(
                            at,
                            LayoutPanel {
                                kind: panel.kind,
                                share,
                            },
                        );
                    }
                    None => {
                        // Split off a new column at the edge, taking half the
                        // source column's width.
                        let share = self.columns[ci].share / 2.0;
                        self.columns[ci].share -= share;
                        let col = LayoutColumn {
                            panels: vec![LayoutPanel {
                                kind: panel.kind,
                                share: 1.0,
                            }],
                            share,
                        };
                        match dir {
                            MoveDir::Left => self.columns.insert(0, col),
                            _ => self.columns.push(col),
                        }
                    }
                }
            }
            _ => {}
        }
        self.sanitize();
    }

    /// Repairs invariants (exactly one of each present kind, chat present, no
    /// empty columns) and renormalizes all shares to sum to 1 per level.
    pub fn sanitize(&mut self) {
        let mut seen = std::collections::HashSet::new();
        for col in &mut self.columns {
            col.panels.retain(|p| seen.insert(p.kind));
        }
        self.columns.retain(|c| !c.panels.is_empty());
        if self.columns.is_empty() {
            *self = Self::single_chat();
            return;
        }
        if !seen.contains(&PanelKind::Chat) {
            for col in &mut self.columns {
                col.share *= 0.5;
            }
            self.columns.insert(
                0,
                LayoutColumn {
                    panels: vec![LayoutPanel {
                        kind: PanelKind::Chat,
                        share: 1.0,
                    }],
                    share: 0.5,
                },
            );
        }
        let total: f32 = self.columns.iter().map(|c| c.share.max(MIN_SHARE)).sum();
        for col in &mut self.columns {
            col.share = col.share.max(MIN_SHARE) / total;
            let sum: f32 = col.panels.iter().map(|p| p.share.max(MIN_SHARE)).sum();
            for p in &mut col.panels {
                p.share = p.share.max(MIN_SHARE) / sum;
            }
        }
    }
}

/// serde default for settings that ship enabled (a `bool` field's plain
/// `#[serde(default)]` would silently turn them off for existing configs).
fn default_true() -> bool {
    true
}

/// One tab's persisted configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TabConfig {
    pub name: String,
    #[serde(default)]
    pub twitch_channel: String,
    #[serde(default)]
    pub kick_channel: String,
    /// The YouTube source: an `@handle`, channel/watch/live URL, or bare video id.
    #[serde(default)]
    pub youtube_channel: String,
    /// Which event kinds appear in the events panel (all enabled by default).
    #[serde(default)]
    pub event_kinds: EventFilter,
    /// Which event kinds play the alert ping on arrival (all off by default).
    #[serde(default)]
    pub event_sounds: EventSounds,
    /// When the events panel is shown, hide event rows from the main chat log so
    /// events appear *only* in the panel. Off by default (events show in both).
    #[serde(default)]
    pub events_only: bool,
    /// Hide a sub/resub's attached chat message in the events panel (the sub
    /// info still shows; the chat log keeps the full event). Off by default.
    #[serde(default)]
    pub hide_sub_messages: bool,
    /// Collapse a mass gift's per-recipient rows in the events panel under its
    /// "gifted N subs" announcement (expandable to the recipient list). On by
    /// default; the chat log always shows the individual rows.
    #[serde(default = "default_true")]
    pub collapse_gift_subs: bool,
    /// The mentions panel shows every tab's mentions (each under a "#channel"
    /// tag that jumps to its tab) instead of just this tab's. Off by default.
    #[serde(default)]
    pub mentions_all_tabs: bool,
    /// This tab's own extra mention terms, added to (union with) the global
    /// `custom_mentions` for highlighting in this tab only. Empty by default.
    #[serde(default)]
    pub custom_mentions: Vec<String>,
    /// This tab's own extra ignore terms, added to (union with) the global
    /// `ignored_terms` — messages matching them are hidden in this tab only
    /// (they stay in the shared buffer so other tabs on the channel still see
    /// them). Empty by default.
    #[serde(default)]
    pub ignored_terms: Vec<String>,
    /// This tab's own extra suppress terms, added to (union with) the global
    /// `suppressed_terms` — messages matching them are dimmed (but still shown)
    /// in this tab only. Empty by default.
    #[serde(default)]
    pub suppressed_terms: Vec<String>,
    /// The panel arrangement (chat/events/mentions grid). A config without one
    /// (or with a broken one) is repaired to defaults on load by
    /// [`Layout::sanitize`] — no migration of old formats.
    #[serde(default)]
    pub layout: Layout,
}

impl TabConfig {
    /// A fresh, unconfigured tab. The name is left blank so it falls back to the
    /// channel-derived [`default_name`](Self::default_name).
    pub fn empty() -> Self {
        Self {
            name: String::new(),
            twitch_channel: String::new(),
            kick_channel: String::new(),
            youtube_channel: String::new(),
            event_kinds: EventFilter::default(),
            event_sounds: EventSounds::default(),
            events_only: false,
            hide_sub_messages: false,
            collapse_gift_subs: true,
            mentions_all_tabs: false,
            custom_mentions: Vec::new(),
            ignored_terms: Vec::new(),
            suppressed_terms: Vec::new(),
            layout: Layout::single_chat(),
        }
    }

    /// Whether this tab has at least one channel to connect to.
    pub fn has_channel(&self) -> bool {
        !self.twitch_channel.trim().is_empty()
            || !self.kick_channel.trim().is_empty()
            || !self.youtube_channel.trim().is_empty()
    }

    /// The label to show on the tab: the user's name if set, else the
    /// channel-derived [`default_name`](Self::default_name).
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            self.default_name()
        } else {
            self.name.clone()
        }
    }

    /// Name to use when the user hasn't set one: the Twitch channel, else the
    /// Kick channel, else the YouTube source, else a generic placeholder.
    pub fn default_name(&self) -> String {
        let twitch = self.twitch_channel.trim();
        let kick = self.kick_channel.trim();
        let youtube = self.youtube_channel.trim();
        if !twitch.is_empty() {
            twitch.to_string()
        } else if !kick.is_empty() {
            kick.to_string()
        } else if !youtube.is_empty() {
            youtube.to_string()
        } else {
            "New Tab".to_string()
        }
    }
}

/// Loads the saved tabs; returns a single empty tab if none are saved (or the
/// file doesn't parse — rapid development, no format migration). A missing or
/// broken layout is repaired to the plain single-chat default.
pub fn load() -> Vec<TabConfig> {
    match bks_auth::store::load::<Vec<TabConfig>>(STORE_NAME) {
        Ok(Some(mut tabs)) if !tabs.is_empty() => {
            for tab in &mut tabs {
                tab.layout.sanitize();
            }
            tabs
        }
        _ => vec![TabConfig::empty()],
    }
}

/// Persists the tab list, logging on failure (not fatal to the UI).
pub fn save(tabs: &[TabConfig]) {
    if let Err(err) = bks_auth::store::save(STORE_NAME, &tabs.to_vec()) {
        tracing::warn!("failed to save tabs: {err:#}");
    }
}

/// Loads the saved active-tab index, clamped to a valid position for the given
/// tab count (so a stale index from a since-removed tab falls back in range).
/// Returns 0 when nothing is saved or no tabs exist.
pub fn load_active(tab_count: usize) -> usize {
    if tab_count == 0 {
        return 0;
    }
    match bks_auth::store::load::<usize>(ACTIVE_STORE_NAME) {
        Ok(Some(ix)) => ix.min(tab_count - 1),
        _ => 0,
    }
}

/// Persists the active-tab index, logging on failure (not fatal to the UI).
pub fn save_active(ix: usize) {
    if let Err(err) = bks_auth::store::save(ACTIVE_STORE_NAME, &ix) {
        tracing::warn!("failed to save active tab: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(layout: &Layout) -> Vec<Vec<PanelKind>> {
        layout
            .columns
            .iter()
            .map(|c| c.panels.iter().map(|p| p.kind).collect())
            .collect()
    }

    /// `[Chat][Events]` (chat plus a side events column).
    fn chat_events() -> Layout {
        let mut l = Layout::single_chat();
        l.set_enabled(PanelKind::Events, true);
        l
    }

    /// `[Chat][Events, Mentions]` (both aux panels stacked in the side column).
    fn chat_events_mentions() -> Layout {
        let mut l = chat_events();
        l.set_enabled(PanelKind::Mentions, true);
        l
    }

    #[test]
    fn broken_layout_sanitizes_to_single_chat() {
        let mut l = Layout::default(); // an empty/unparsed layout
        l.sanitize();
        assert_eq!(kinds(&l), vec![vec![PanelKind::Chat]]);
    }

    #[test]
    fn enable_joins_sibling_column_and_disable_drops_empty_column() {
        let mut l = Layout::single_chat();
        l.set_enabled(PanelKind::Events, true);
        assert_eq!(
            kinds(&l),
            vec![vec![PanelKind::Chat], vec![PanelKind::Events]]
        );
        l.set_enabled(PanelKind::Mentions, true);
        assert_eq!(
            kinds(&l),
            vec![
                vec![PanelKind::Chat],
                vec![PanelKind::Events, PanelKind::Mentions]
            ]
        );
        l.set_enabled(PanelKind::Events, false);
        l.set_enabled(PanelKind::Mentions, false);
        assert_eq!(kinds(&l), vec![vec![PanelKind::Chat]]);
        // Chat can't be hidden.
        l.set_enabled(PanelKind::Chat, false);
        assert!(l.contains(PanelKind::Chat));
    }

    #[test]
    fn move_left_joins_then_splits_off_edge_column() {
        let mut l = chat_events();
        // [Chat][Events]: ◀ joins chat's column (stacks above it)…
        l.move_panel(PanelKind::Events, MoveDir::Left);
        assert_eq!(kinds(&l), vec![vec![PanelKind::Events, PanelKind::Chat]]);
        // …and ◀ again breaks out into its own leftmost column.
        l.move_panel(PanelKind::Events, MoveDir::Left);
        assert_eq!(
            kinds(&l),
            vec![vec![PanelKind::Events], vec![PanelKind::Chat]]
        );
        // Alone at the left edge: no-op.
        l.move_panel(PanelKind::Events, MoveDir::Left);
        assert_eq!(
            kinds(&l),
            vec![vec![PanelKind::Events], vec![PanelKind::Chat]]
        );
    }

    #[test]
    fn move_up_down_swap_within_column() {
        let mut l = chat_events_mentions();
        l.move_panel(PanelKind::Mentions, MoveDir::Up);
        assert_eq!(kinds(&l)[1], vec![PanelKind::Mentions, PanelKind::Events]);
        l.move_panel(PanelKind::Mentions, MoveDir::Down);
        assert_eq!(kinds(&l)[1], vec![PanelKind::Events, PanelKind::Mentions]);
        // At the top already: no-op.
        l.move_panel(PanelKind::Events, MoveDir::Up);
        assert_eq!(kinds(&l)[1], vec![PanelKind::Events, PanelKind::Mentions]);
    }

    #[test]
    fn sanitize_restores_chat_and_normalizes_shares() {
        let mut l = Layout {
            columns: vec![LayoutColumn {
                panels: vec![
                    LayoutPanel {
                        kind: PanelKind::Events,
                        share: 3.0,
                    },
                    LayoutPanel {
                        kind: PanelKind::Events, // duplicate: dropped
                        share: 1.0,
                    },
                ],
                share: 2.0,
            }],
        };
        l.sanitize();
        assert!(l.contains(PanelKind::Chat));
        let col_sum: f32 = l.columns.iter().map(|c| c.share).sum();
        assert!((col_sum - 1.0).abs() < 1e-5);
        for col in &l.columns {
            let sum: f32 = col.panels.iter().map(|p| p.share).sum();
            assert!((sum - 1.0).abs() < 1e-5);
            assert_eq!(
                col.panels
                    .iter()
                    .filter(|p| p.kind == PanelKind::Events)
                    .count()
                    .max(1),
                1
            );
        }
    }
}
