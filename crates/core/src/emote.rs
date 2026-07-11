use serde::{Deserialize, Serialize};

/// The facts a tooltip needs about an emote, kept structured so the UI does the
/// (single, consistent) formatting — only the emote's *source* knows the provider
/// and author, so it fills these in; render turns them into the hover text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmoteTooltip {
    /// Where the emote comes from, shown as "<provider> Emote" (e.g. "Twitch",
    /// "7TV", "Kick"). Empty means no provider line.
    #[serde(default)]
    pub provider: String,
    /// The emote's creator, shown as "By: <author>" (7TV exposes this; native
    /// Twitch/Kick emotes don't). `None` omits the line.
    #[serde(default)]
    pub author: Option<String>,
}

impl EmoteTooltip {
    /// A tooltip for a native platform emote (no author): just the provider line.
    pub fn provider(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            author: None,
        }
    }
}

/// A resolved emote ready to render: a name to show on hover and a URL to load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emote {
    pub id: String,
    pub name: String,
    pub url: String,
    pub animated: bool,
    /// Source facts (provider, author) the UI formats into the hover tooltip.
    #[serde(default)]
    pub tooltip: EmoteTooltip,
}
