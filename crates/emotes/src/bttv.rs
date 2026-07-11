//! BetterTTV (BTTV) emote provider. Fetches global + per-channel emote sets from
//! the BTTV v3 REST API and maps them to [`Emote`]s. Twitch-only: the channel
//! lookup keys on the Twitch numeric `room-id` (same id 7TV uses). Mirrors the
//! C++ Backseater's BTTV handling.

use async_trait::async_trait;
use bks_core::Emote;
use serde::Deserialize;

use crate::http::{fetch_cached, shared_client};
use crate::EmoteProvider;

const GLOBAL_URL: &str = "https://api.betterttv.net/3/cached/emotes/global";
const CHANNEL_URL: &str = "https://api.betterttv.net/3/cached/users/twitch/";

/// One BTTV emote. `code` is the name shown in chat. `image_type` is `png`/`gif`;
/// older responses omit `animated`, so animation is inferred from either.
#[derive(Deserialize)]
struct BttvEmote {
    id: String,
    code: String,
    #[serde(default, rename = "imageType")]
    image_type: String,
    #[serde(default)]
    animated: bool,
    /// Present only on shared emotes — the uploader, credited in the tooltip.
    #[serde(default)]
    user: Option<BttvUser>,
}

#[derive(Deserialize)]
struct BttvUser {
    #[serde(default, rename = "displayName")]
    display_name: String,
}

/// The per-channel response: the channel's own emotes plus emotes shared into it.
#[derive(Deserialize)]
struct ChannelResponse {
    #[serde(default, rename = "channelEmotes")]
    channel_emotes: Vec<BttvEmote>,
    #[serde(default, rename = "sharedEmotes")]
    shared_emotes: Vec<BttvEmote>,
}

/// Twitch-only BTTV provider.
pub struct BttvProvider {
    client: reqwest::Client,
}

impl BttvProvider {
    pub fn new() -> Self {
        Self {
            client: shared_client(),
        }
    }
}

impl Default for BttvProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// The CDN URL for a BTTV emote, sized to the display scale (1x at 100% DPI, 2x
/// above) to keep download/decode/heap small. BTTV serves 1x/2x/3x.
fn image_url(id: &str) -> String {
    let size = if crate::http::preferred_scale() >= 2 {
        "2x"
    } else {
        "1x"
    };
    format!("https://cdn.betterttv.net/emote/{id}/{size}.webp")
}

fn to_emote(e: BttvEmote) -> Emote {
    // BTTV marks animation either via the explicit flag or a `gif` image type.
    let animated = e.animated || e.image_type.eq_ignore_ascii_case("gif");
    let author = e.user.map(|u| u.display_name).filter(|n| !n.is_empty());
    Emote {
        url: image_url(&e.id),
        id: e.id,
        name: e.code,
        animated,
        tooltip: bks_core::EmoteTooltip {
            provider: "BTTV".into(),
            author,
        },
    }
}

fn collect(emotes: Vec<BttvEmote>) -> Vec<Emote> {
    emotes.into_iter().map(to_emote).collect()
}

#[async_trait]
impl EmoteProvider for BttvProvider {
    fn name(&self) -> &'static str {
        "BTTV"
    }

    async fn load_global(&self) -> anyhow::Result<Vec<Emote>> {
        fetch_cached(&self.client, GLOBAL_URL, false, collect).await
    }

    async fn load_channel(&self, channel_id: &str) -> anyhow::Result<Vec<Emote>> {
        let url = format!("{CHANNEL_URL}{channel_id}");
        // A channel with no BTTV emotes returns 404; treated as "no emotes".
        fetch_cached(&self.client, &url, true, |body: ChannelResponse| {
            let mut emotes = collect(body.channel_emotes);
            emotes.extend(collect(body.shared_emotes));
            emotes
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::scale_test_guard as scale_guard;

    #[test]
    fn parses_global_array_with_mixed_animation_signals() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        let json = r#"[
            {"id":"1","code":"SourPls","imageType":"gif"},
            {"id":"2","code":"OhMyGoodness","imageType":"png"},
            {"id":"3","code":"NewFlag","imageType":"png","animated":true}
        ]"#;
        let parsed: Vec<BttvEmote> = serde_json::from_str(json).unwrap();
        let emotes = collect(parsed);
        assert_eq!(emotes.len(), 3);
        assert_eq!(emotes[0].name, "SourPls");
        assert_eq!(emotes[0].url, "https://cdn.betterttv.net/emote/1/1x.webp");
        assert!(emotes[0].animated); // imageType gif
        assert!(!emotes[1].animated); // png, no flag
        assert!(emotes[2].animated); // explicit animated flag
        assert_eq!(emotes[0].tooltip.provider, "BTTV");
        assert_eq!(emotes[0].tooltip.author, None);
    }

    #[test]
    fn parses_channel_merges_channel_and_shared_with_author() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        let json = r#"{
            "channelEmotes":[{"id":"c1","code":"ChanEmote","imageType":"png"}],
            "sharedEmotes":[{"id":"s1","code":"Shared","imageType":"gif","user":{"displayName":"Alice"}}]
        }"#;
        let body: ChannelResponse = serde_json::from_str(json).unwrap();
        let mut emotes = collect(body.channel_emotes);
        emotes.extend(collect(body.shared_emotes));
        assert_eq!(emotes.len(), 2);
        // Channel emote: no user → no author.
        assert_eq!(emotes[0].name, "ChanEmote");
        assert_eq!(emotes[0].tooltip.author, None);
        // Shared emote: animated gif, credited to its uploader.
        assert_eq!(emotes[1].name, "Shared");
        assert!(emotes[1].animated);
        assert_eq!(emotes[1].tooltip.provider, "BTTV");
        assert_eq!(emotes[1].tooltip.author.as_deref(), Some("Alice"));
        assert_eq!(emotes[1].url, "https://cdn.betterttv.net/emote/s1/1x.webp");
    }
}
