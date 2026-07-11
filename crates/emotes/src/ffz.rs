//! FrankerFaceZ (FFZ) emote provider. Fetches global + per-channel emote sets
//! from the FFZ v1 REST API and maps them to [`Emote`]s. Twitch-only: the channel
//! lookup keys on the Twitch numeric `room-id` (same id 7TV uses).

use std::collections::HashMap;

use async_trait::async_trait;
use bks_core::Emote;
use serde::Deserialize;

use crate::http::{fetch_cached, shared_client};
use crate::EmoteProvider;

const GLOBAL_URL: &str = "https://api.frankerfacez.com/v1/set/global";
const CHANNEL_URL: &str = "https://api.frankerfacez.com/v1/room/id/";

/// One FFZ emote. `urls` maps a size key ("1"/"2"/"4") to a protocol-relative
/// CDN path; `animated`, when present, maps size keys to animated WEBP variants.
#[derive(Deserialize)]
struct Emoticon {
    id: u64,
    name: String,
    #[serde(default)]
    urls: HashMap<String, String>,
    /// Present only on animated emotes; its mere presence marks animation.
    #[serde(default)]
    animated: Option<HashMap<String, String>>,
    #[serde(default)]
    owner: Option<Owner>,
}

#[derive(Deserialize)]
struct Owner {
    #[serde(default)]
    display_name: String,
}

/// One emote set: FFZ groups emotes into sets, listed globally under
/// `default_sets` and per-room under `sets`.
#[derive(Deserialize)]
struct EmoteSet {
    #[serde(default)]
    emoticons: Vec<Emoticon>,
}

#[derive(Deserialize)]
struct GlobalResponse {
    #[serde(default)]
    default_sets: Vec<u64>,
    #[serde(default)]
    sets: HashMap<String, EmoteSet>,
}

#[derive(Deserialize)]
struct RoomResponse {
    #[serde(default)]
    sets: HashMap<String, EmoteSet>,
}

/// Twitch-only FFZ provider.
pub struct FfzProvider {
    client: reqwest::Client,
}

impl FfzProvider {
    pub fn new() -> Self {
        Self {
            client: shared_client(),
        }
    }
}

impl Default for FfzProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Picks the largest available size ("4" → "2" → "1") and returns an absolute
/// `https:` URL. FFZ now returns absolute `https://` URLs, but older/cached
/// responses use protocol-relative `//` paths, so only those get prefixed.
/// `None` if no size is offered.
fn largest_url(urls: &HashMap<String, String>) -> Option<String> {
    // Match the display scale: 1x at 100% DPI, 2x above. Keeps
    // downloads/decode/heap small; falls back to whatever's offered. FFZ sizes are
    // "1"/"2"/"4".
    let order: [&str; 3] = if crate::http::preferred_scale() >= 2 {
        ["2", "4", "1"]
    } else {
        ["1", "2", "4"]
    };
    order.iter().find_map(|k| urls.get(*k)).map(|u| {
        if u.starts_with("//") {
            format!("https:{u}")
        } else {
            u.clone()
        }
    })
}

fn to_emote(e: Emoticon) -> Option<Emote> {
    let url = largest_url(&e.urls)?;
    // FFZ only attaches an `animated` map to animated emotes, so its presence is
    // the animation signal (we still render the static `urls` for simplicity).
    let animated = e.animated.is_some();
    let author = e.owner.map(|o| o.display_name).filter(|n| !n.is_empty());
    Some(Emote {
        id: e.id.to_string(),
        name: e.name,
        url,
        animated,
        tooltip: bks_core::EmoteTooltip {
            provider: "FFZ".into(),
            author,
        },
    })
}

/// Flattens the emoticons of the given sets into resolved emotes.
fn collect(sets: impl IntoIterator<Item = EmoteSet>) -> Vec<Emote> {
    sets.into_iter()
        .flat_map(|s| s.emoticons)
        .filter_map(to_emote)
        .collect()
}

/// The emote sets named in `default_sets`, removed from `sets` so they can be
/// consumed by value (FFZ only the listed sets are the active global emotes).
fn active_global_sets(body: GlobalResponse) -> Vec<EmoteSet> {
    let GlobalResponse {
        default_sets,
        mut sets,
    } = body;
    default_sets
        .iter()
        .filter_map(|id| sets.remove(&id.to_string()))
        .collect()
}

#[async_trait]
impl EmoteProvider for FfzProvider {
    fn name(&self) -> &'static str {
        "FFZ"
    }

    async fn load_global(&self) -> anyhow::Result<Vec<Emote>> {
        fetch_cached(&self.client, GLOBAL_URL, false, |body: GlobalResponse| {
            collect(active_global_sets(body))
        })
        .await
    }

    async fn load_channel(&self, channel_id: &str) -> anyhow::Result<Vec<Emote>> {
        let url = format!("{CHANNEL_URL}{channel_id}");
        // A channel with no FFZ room returns 404; treated as "no emotes".
        fetch_cached(&self.client, &url, true, |body: RoomResponse| {
            collect(body.sets.into_values())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::http::scale_test_guard as scale_guard;

    #[test]
    fn largest_url_size_by_scale() {
        let _g = scale_guard();
        let mut urls = HashMap::new();
        urls.insert(
            "1".to_string(),
            "//cdn.frankerfacez.com/emote/1/1".to_string(),
        );
        urls.insert(
            "2".to_string(),
            "//cdn.frankerfacez.com/emote/1/2".to_string(),
        );
        urls.insert(
            "4".to_string(),
            "//cdn.frankerfacez.com/emote/1/4".to_string(),
        );

        // At 100% DPI: prefer 1x (smallest, exact at native render size).
        bks_core::set_preferred_scale(1);
        assert_eq!(
            largest_url(&urls).as_deref(),
            Some("https://cdn.frankerfacez.com/emote/1/1")
        );
        // At HiDPI: prefer 2x.
        bks_core::set_preferred_scale(2);
        assert_eq!(
            largest_url(&urls).as_deref(),
            Some("https://cdn.frankerfacez.com/emote/1/2")
        );
        bks_core::set_preferred_scale(1); // restore default
        assert_eq!(largest_url(&HashMap::new()), None);
    }

    #[test]
    fn largest_url_leaves_absolute_urls_intact() {
        // The live FFZ API now returns absolute https URLs; they must not get a
        // second `https:` prefix (that produced unloadable `https:https://...`).
        let mut urls = HashMap::new();
        urls.insert(
            "4".to_string(),
            "https://cdn.frankerfacez.com/emote/1/4".to_string(),
        );
        assert_eq!(
            largest_url(&urls).as_deref(),
            Some("https://cdn.frankerfacez.com/emote/1/4")
        );
    }

    #[test]
    fn parses_global_only_from_default_sets() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        let json = r#"{
            "default_sets":[3],
            "sets":{
                "3":{"emoticons":[
                    {"id":1,"name":"ZreknarF","urls":{"1":"//cdn.frankerfacez.com/emote/1/1","4":"//cdn.frankerfacez.com/emote/1/4"},"owner":{"display_name":"Zrek"}},
                    {"id":2,"name":"Animated","urls":{"1":"//cdn.frankerfacez.com/emote/2/1"},"animated":{"1":"//cdn.frankerfacez.com/emote/2/animated/1"}}
                ]},
                "99":{"emoticons":[{"id":9,"name":"NotActive","urls":{"1":"//x/9"}}]}
            }
        }"#;
        let body: GlobalResponse = serde_json::from_str(json).unwrap();
        let emotes = collect(active_global_sets(body));
        // Only set 3's two emotes; set 99 isn't in default_sets.
        assert_eq!(emotes.len(), 2);
        let first = &emotes[0];
        assert_eq!(first.name, "ZreknarF");
        // Offers 1 and 4 but no 2 → preference falls to 1 (smaller download than 4).
        assert_eq!(first.url, "https://cdn.frankerfacez.com/emote/1/1");
        assert!(!first.animated);
        assert_eq!(first.tooltip.provider, "FFZ");
        assert_eq!(first.tooltip.author.as_deref(), Some("Zrek"));
        // Second emote has an `animated` map → animated.
        assert!(emotes[1].animated);
        assert_eq!(emotes[1].tooltip.author, None);
    }

    #[test]
    fn parses_room_from_all_sets() {
        let json = r#"{
            "room":{"id":12345},
            "sets":{
                "100":{"emoticons":[{"id":7,"name":"RoomEmote","urls":{"2":"//cdn.frankerfacez.com/emote/7/2"}}]}
            }
        }"#;
        let body: RoomResponse = serde_json::from_str(json).unwrap();
        let emotes = collect(body.sets.into_values());
        assert_eq!(emotes.len(), 1);
        assert_eq!(emotes[0].name, "RoomEmote");
        assert_eq!(emotes[0].id, "7");
        assert_eq!(emotes[0].url, "https://cdn.frankerfacez.com/emote/7/2");
    }
}
