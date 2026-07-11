use std::collections::HashMap;
use std::sync::Arc;

use bks_core::{Emote, MessageElement};

use crate::EmoteProvider;

/// A name → emote map. Emotes are [`Arc`]-interned so the same emote can be
/// shared across the global set, multiple channels, and every message it's
/// resolved into without copying its three `String`s.
pub type EmoteMap = HashMap<String, Arc<Emote>>;

/// A registry of resolvable emotes for one connection (tab).
///
/// The **global** set (FFZ/BTTV/7TV globals) is identical for every tab, so it's
/// held behind a shared [`Arc`] — loaded once app-wide and pointed at by every
/// registry (see [`with_globals`](Self::with_globals)) rather than copied per
/// tab. Only the tab's **channel** emotes are owned here. `resolve` checks the
/// channel set first, then falls back to the shared globals (a channel emote
/// shadows a global of the same name).
#[derive(Default, Clone)]
pub struct EmoteRegistry {
    /// Shared, app-wide global emotes. Empty until set via [`with_globals`].
    global: Arc<EmoteMap>,
    per_channel: HashMap<String, EmoteMap>,
}

impl EmoteRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry that resolves against the given shared global set (loaded once
    /// app-wide). Channel emotes are still loaded per registry.
    pub fn with_globals(global: Arc<EmoteMap>) -> Self {
        Self {
            global,
            per_channel: HashMap::new(),
        }
    }

    /// Inserts a global emote into a standalone (non-shared) global map, keeping
    /// the existing one on a name collision so the first provider loaded wins
    /// (see [`load_providers`](Self::load_providers)). Only valid before the
    /// global set is shared; used when building the app-wide global registry.
    pub fn insert_global(&mut self, emote: Emote) {
        Arc::make_mut(&mut self.global)
            .entry(emote.name.clone())
            .or_insert_with(|| Arc::new(emote));
    }

    /// Inserts a channel emote, keeping the existing one on a name collision.
    pub fn insert_channel(&mut self, channel: &str, emote: Emote) {
        self.per_channel
            .entry(channel.to_string())
            .or_default()
            .entry(emote.name.clone())
            .or_insert_with(|| Arc::new(emote));
    }

    /// The shared global set, e.g. to seed another registry via
    /// [`with_globals`](Self::with_globals).
    pub fn globals(&self) -> Arc<EmoteMap> {
        Arc::clone(&self.global)
    }

    pub fn resolve(&self, channel: &str, word: &str) -> Option<&Arc<Emote>> {
        self.per_channel
            .get(channel)
            .and_then(|m| m.get(word))
            .or_else(|| self.global.get(word))
    }

    /// True if any emotes are loaded yet. The bridge skips resolution until then.
    pub fn is_empty(&self) -> bool {
        self.global.is_empty() && self.per_channel.is_empty()
    }

    /// Every emote usable in `channel` (its channel set plus the globals),
    /// sorted by name, for the UI's emote picker. A channel emote shadows a
    /// global of the same name (matching [`resolve`](Self::resolve)).
    pub fn emotes(&self, channel: &str) -> Vec<Arc<Emote>> {
        let mut by_name: HashMap<&str, &Arc<Emote>> = HashMap::new();
        for emote in self.global.values() {
            by_name.insert(&emote.name, emote);
        }
        if let Some(channel_emotes) = self.per_channel.get(channel) {
            for emote in channel_emotes.values() {
                by_name.insert(&emote.name, emote);
            }
        }
        let mut emotes: Vec<Arc<Emote>> = by_name.into_values().map(Arc::clone).collect();
        // `sort_by_cached_key` lowercases each name exactly once (vs. O(n log n)
        // allocations from `sort_by_key` recomputing the key per comparison).
        emotes.sort_by_cached_key(|e| e.name.to_lowercase());
        emotes
    }

    /// Rewrites a message's element stream, splitting each [`Text`] run on
    /// whitespace and replacing any word that matches a known emote with an
    /// [`Emote`] element. Native Twitch emotes (already [`Emote`]s) pass through
    /// untouched, so this only fills in 3rd-party (7TV/BTTV/FFZ) emotes.
    ///
    /// [`Text`]: MessageElement::Text
    /// [`Emote`]: MessageElement::Emote
    pub fn resolve_elements(
        &self,
        channel: &str,
        elements: Vec<MessageElement>,
    ) -> Vec<MessageElement> {
        let mut out = Vec::with_capacity(elements.len());
        for element in elements {
            let MessageElement::Text { text, color } = element else {
                out.push(element);
                continue;
            };
            // Re-emit alternating non-emote text and emote tokens, preserving the
            // original spacing between words so runs read naturally.
            let mut pending = String::new();
            let mut cursor = 0;
            for (offset, word) in split_word_indices(&text) {
                if let Some(emote) = self.resolve(channel, word) {
                    pending.push_str(&text[cursor..offset]);
                    if !pending.is_empty() {
                        out.push(MessageElement::Text {
                            text: std::mem::take(&mut pending),
                            color,
                        });
                    }
                    out.push(MessageElement::Emote(Arc::clone(emote)));
                    cursor = offset + word.len();
                }
            }
            pending.push_str(&text[cursor..]);
            if !pending.is_empty() {
                out.push(MessageElement::Text {
                    text: pending,
                    color,
                });
            }
        }
        out
    }

    /// Loads every provider's *global* set into this registry, building the
    /// shared app-wide global map (call once at startup, then hand
    /// [`globals`](Self::globals) to per-tab registries). Earlier providers win
    /// name collisions. Logs each set's size — fired once, not per tab.
    ///
    /// The fetches run concurrently (they don't touch `self`); results are then
    /// inserted in provider order so the earlier-wins collision rule holds.
    pub async fn load_globals(&mut self, providers: &[Box<dyn EmoteProvider>]) {
        let fetched = futures_util::future::join_all(providers.iter().map(|p| p.load_global())).await;
        for (provider, result) in providers.iter().zip(fetched) {
            match result {
                Ok(global) => {
                    let count = global.len();
                    for emote in global {
                        self.insert_global(emote);
                    }
                    tracing::info!("loaded {count} global {} emotes", provider.name());
                }
                Err(err) => {
                    tracing::warn!("failed to load global {} emotes: {err:#}", provider.name())
                }
            }
        }
    }

    /// Loads every provider's *channel* emotes for the channel, accumulating into
    /// this registry (globals already come shared). Earlier providers win name
    /// collisions. One provider failing is logged and does not abort the rest.
    ///
    /// The per-provider fetches run concurrently; results are inserted in provider
    /// order so the earlier-wins rule holds.
    ///
    /// Returns the number of providers that loaded without error.
    pub async fn load_providers(
        &mut self,
        providers: &[Box<dyn EmoteProvider>],
        channel: &str,
        fetch_id: &str,
    ) -> usize {
        let fetched =
            futures_util::future::join_all(providers.iter().map(|p| p.load_channel(fetch_id))).await;
        let mut loaded = 0;
        for (provider, result) in providers.iter().zip(fetched) {
            match result {
                Ok(channel_emotes) => {
                    let count = channel_emotes.len();
                    for emote in channel_emotes {
                        self.insert_channel(channel, emote);
                    }
                    tracing::debug!(
                        "loaded {count} channel {} emotes for {channel}",
                        provider.name()
                    );
                    loaded += 1;
                }
                Err(err) => {
                    tracing::warn!("failed to load {} emotes: {err:#}", provider.name())
                }
            }
        }
        loaded
    }
}

/// Yields `(byte_offset, word)` for each whitespace-delimited word in `text`,
/// where `word` is a borrowed slice so its byte length maps back into `text`.
fn split_word_indices(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.split_whitespace()
        .map(move |word| (word.as_ptr() as usize - text.as_ptr() as usize, word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bks_core::{Color, Emote};

    fn emote(name: &str) -> Emote {
        Emote {
            id: name.to_string(),
            name: name.to_string(),
            url: format!("https://cdn/{name}.webp"),
            animated: false,
            tooltip: Default::default(),
        }
    }

    fn kinds(elements: &[MessageElement]) -> Vec<String> {
        elements
            .iter()
            .map(|e| match e {
                MessageElement::Text { text, .. } => format!("T:{text}"),
                MessageElement::Emote(em) => format!("E:{}", em.name),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn resolves_global_emote_in_text_run() {
        let mut reg = EmoteRegistry::new();
        reg.insert_global(emote("trsLove"));
        let els = reg.resolve_elements(
            "posty",
            vec![MessageElement::Text {
                text: "hello trsLove world".into(),
                color: None,
            }],
        );
        assert_eq!(kinds(&els), vec!["T:hello ", "E:trsLove", "T: world"]);
    }

    #[test]
    fn channel_emote_shadows_and_non_text_passes_through() {
        let mut reg = EmoteRegistry::new();
        reg.insert_channel("posty", emote("postySmash"));
        let els = reg.resolve_elements(
            "posty",
            vec![
                MessageElement::Emote(std::sync::Arc::new(emote("Kappa"))),
                MessageElement::Text {
                    text: "postySmash".into(),
                    color: Some(Color::rgb(1, 2, 3)),
                },
            ],
        );
        assert_eq!(kinds(&els), vec!["E:Kappa", "E:postySmash"]);
    }

    #[test]
    fn emotes_lists_channel_then_global_sorted_and_deduped() {
        let mut reg = EmoteRegistry::new();
        reg.insert_global(emote("Kappa"));
        reg.insert_global(emote("zzz"));
        reg.insert_channel("posty", emote("postySmash"));
        // A channel emote shadows the global of the same name.
        reg.insert_global(emote("postySmash"));

        let names: Vec<String> = reg
            .emotes("posty")
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["Kappa", "postySmash", "zzz"]);
        // Another channel doesn't see posty's channel emotes.
        let other: Vec<String> = reg.emotes("other").iter().map(|e| e.name.clone()).collect();
        assert_eq!(other, vec!["Kappa", "postySmash", "zzz"]);
    }

    #[test]
    fn text_without_known_emotes_is_unchanged() {
        let mut reg = EmoteRegistry::new();
        reg.insert_global(emote("trsLove"));
        let els = reg.resolve_elements(
            "posty",
            vec![MessageElement::Text {
                text: "just plain words".into(),
                color: None,
            }],
        );
        assert_eq!(kinds(&els), vec!["T:just plain words"]);
    }
}
