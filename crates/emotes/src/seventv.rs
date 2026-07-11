//! 7TV emote provider. Fetches global + per-channel emote sets from the 7TV v3
//! REST API and maps them to [`Emote`]s. Mirrors the C++ Backseater's
//! `SeventvEmotes`: global comes from `/emote-sets/global`, channel emotes from
//! `/users/{platform}/{id}` (Twitch uses the numeric `room-id`; Kick uses the
//! channel's numeric user id). Global emotes are shared across platforms.

use async_trait::async_trait;
use bks_core::Emote;
use serde::Deserialize;

use crate::http::{fetch_cached, shared_client};
use crate::EmoteProvider;

const GLOBAL_URL: &str = "https://7tv.io/v3/emote-sets/global";
const TWITCH_USER_URL: &str = "https://7tv.io/v3/users/twitch/";
const KICK_USER_URL: &str = "https://7tv.io/v3/users/kick/";
const YOUTUBE_USER_URL: &str = "https://7tv.io/v3/users/youtube/";

/// One entry in an emote set's `emotes` array. `name` is the (possibly aliased)
/// name shown in chat; `data` carries the image host shared across aliases.
#[derive(Deserialize)]
struct ActiveEmote {
    id: String,
    name: String,
    data: Option<EmoteData>,
}

#[derive(Deserialize)]
struct EmoteData {
    #[serde(default)]
    animated: bool,
    host: Host,
    /// The emote's creator, shown in the tooltip as "By: <name>". Absent for some
    /// emotes (deleted/anonymous owners), in which case the line is omitted.
    #[serde(default)]
    owner: Option<Owner>,
}

#[derive(Deserialize)]
pub(crate) struct Owner {
    #[serde(default)]
    pub(crate) display_name: String,
    #[serde(default)]
    pub(crate) username: String,
}

impl Owner {
    /// The name to credit: the display name if set, else the username; `None` if
    /// neither is present (so the tooltip drops the "By:" line).
    pub(crate) fn name(self) -> Option<String> {
        let name = if self.display_name.is_empty() {
            self.username
        } else {
            self.display_name
        };
        (!name.is_empty()).then_some(name)
    }
}

#[derive(Deserialize)]
pub(crate) struct Host {
    /// Protocol-relative base, e.g. `//cdn.7tv.app/emote/<id>`.
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) files: Vec<File>,
}

#[derive(Deserialize)]
pub(crate) struct File {
    pub(crate) name: String,
    pub(crate) format: String,
}

#[derive(Deserialize)]
struct EmoteSet {
    #[serde(default)]
    emotes: Vec<ActiveEmote>,
}

#[derive(Deserialize)]
struct UserResponse {
    emote_set: Option<EmoteSet>,
}

pub struct SeventvProvider {
    client: reqwest::Client,
    /// The platform segment of the per-channel user lookup URL, e.g.
    /// `TWITCH_USER_URL` or `KICK_USER_URL`. Global emotes are platform-agnostic.
    user_url: &'static str,
}

impl SeventvProvider {
    /// A provider that resolves channel emotes by Twitch numeric id.
    pub fn new() -> Self {
        Self {
            client: shared_client(),
            user_url: TWITCH_USER_URL,
        }
    }

    /// A provider that resolves channel emotes by Kick numeric user id.
    pub fn for_kick() -> Self {
        Self {
            client: shared_client(),
            user_url: KICK_USER_URL,
        }
    }

    /// A provider that resolves channel emotes by YouTube channel id (`UC…`).
    pub fn for_youtube() -> Self {
        Self {
            client: shared_client(),
            user_url: YOUTUBE_USER_URL,
        }
    }
}

impl Default for SeventvProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Preferred emote sizes, most-wanted first, for the display's scale. 7TV serves
/// `1x`–`4x`; we match the render size: at 100% DPI chat renders
/// emotes ~26px so `1x` (~32px) is exact and smallest; at >100% we prefer `2x`. We
/// avoid `4x`/`3x` by default — 4–16× the bytes (1–5 MB animated GIFs) + decode +
/// heap for no visible gain. Falls back up/down across sizes if the ideal is absent.
fn size_preference() -> [&'static str; 4] {
    if crate::http::preferred_scale() >= 2 {
        ["2x", "3x", "1x", "4x"]
    } else {
        ["1x", "2x", "3x", "4x"]
    }
}

/// Picks the best file of `format` by [`size_preference`] and builds an absolute
/// `https:` URL. Falls back across sizes (a smaller real file beats none), and if no
/// filename matches a known size prefix, takes the last (largest) of that format.
/// Returns `None` if no file of `format` exists at all.
pub(crate) fn largest_url(host: &Host, format: &str) -> Option<String> {
    let matches_format = |f: &&File| f.format.eq_ignore_ascii_case(format);
    let file = size_preference()
        .iter()
        .find_map(|size| {
            host.files
                .iter()
                .filter(matches_format)
                .find(|f| f.name.starts_with(size))
        })
        .or_else(|| host.files.iter().rfind(matches_format))?;
    Some(format!("https:{}/{}", host.url, file.name))
}

/// Chooses the best renderable URL for an emote, preferring **WEBP** for both static
/// and animated (it's smaller on the wire/disk than GIF — no 256-color palette, better
/// compression), falling back to GIF if WEBP isn't offered. The `animated` flag no
/// longer changes the format: our patched gpui decodes + animates animated WEBP via the
/// same frame-cycling path as GIF (the old "gpui only animates GIF" limitation was the
/// pre-patch frame-advance gating, now removed). We skip AVIF — gpui can't decode it.
pub(crate) fn best_image_url(host: &Host, _animated: bool) -> Option<String> {
    largest_url(host, "WEBP").or_else(|| largest_url(host, "GIF"))
}

fn to_emote(active: ActiveEmote) -> Option<Emote> {
    let data = active.data?;
    let url = best_image_url(&data.host, data.animated)?;
    let author = data.owner.and_then(Owner::name);
    Some(Emote {
        id: active.id,
        name: active.name,
        url,
        animated: data.animated,
        tooltip: bks_core::EmoteTooltip {
            provider: "7TV".into(),
            author,
        },
    })
}

fn collect(emotes: Vec<ActiveEmote>) -> Vec<Emote> {
    emotes.into_iter().filter_map(to_emote).collect()
}

#[async_trait]
impl EmoteProvider for SeventvProvider {
    fn name(&self) -> &'static str {
        "7TV"
    }

    async fn load_global(&self) -> anyhow::Result<Vec<Emote>> {
        // The global set is identical for every channel/connection — the shared
        // cache serves it after the first fetch (at most once per TTL).
        fetch_cached(&self.client, GLOBAL_URL, false, |set: EmoteSet| {
            collect(set.emotes)
        })
        .await
    }

    async fn load_channel(&self, channel_id: &str) -> anyhow::Result<Vec<Emote>> {
        let url = format!("{}{channel_id}", self.user_url);
        // Cached per channel URL so a reconnect (e.g. a login flip re-joining
        // every tab) reuses the set instead of re-fetching it. A channel with no
        // 7TV account returns 404; treated as "no emotes".
        fetch_cached(&self.client, &url, true, |user: UserResponse| {
            user.emote_set
                .map(|s| collect(s.emotes))
                .unwrap_or_default()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::http::scale_test_guard as scale_guard;

    fn host(files: &[(&str, &str)]) -> Host {
        Host {
            url: "//cdn.7tv.app/emote/abc".into(),
            files: files
                .iter()
                .map(|(name, format)| File {
                    name: (*name).into(),
                    format: (*format).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn static_emote_prefers_1x_webp_at_100_dpi() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        // At 100% DPI we pick 1x (smallest, exact at native render size).
        let h = host(&[
            ("1x.webp", "WEBP"),
            ("2x.webp", "WEBP"),
            ("4x.webp", "WEBP"),
        ]);
        assert_eq!(
            best_image_url(&h, false).as_deref(),
            Some("https://cdn.7tv.app/emote/abc/1x.webp")
        );
    }

    #[test]
    fn static_emote_prefers_2x_webp_at_hidpi() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(2);
        let h = host(&[
            ("1x.webp", "WEBP"),
            ("2x.webp", "WEBP"),
            ("4x.webp", "WEBP"),
        ]);
        assert_eq!(
            best_image_url(&h, false).as_deref(),
            Some("https://cdn.7tv.app/emote/abc/2x.webp")
        );
        bks_core::set_preferred_scale(1); // restore default for other tests
    }

    #[test]
    fn animated_emote_prefers_webp_over_gif() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        // Both formats offered → prefer WEBP (smaller), at the 1x size.
        let h = host(&[
            ("1x.webp", "WEBP"),
            ("2x.webp", "WEBP"),
            ("1x.gif", "GIF"),
            ("2x.gif", "GIF"),
        ]);
        assert_eq!(
            best_image_url(&h, true).as_deref(),
            Some("https://cdn.7tv.app/emote/abc/1x.webp")
        );
    }

    #[test]
    fn falls_back_to_gif_without_webp() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        // Only GIF offered → use it.
        let h = host(&[("1x.gif", "GIF"), ("2x.gif", "GIF")]);
        assert_eq!(
            best_image_url(&h, true).as_deref(),
            Some("https://cdn.7tv.app/emote/abc/1x.gif")
        );
    }

    #[test]
    fn falls_back_to_4x_when_no_smaller_size() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        // Only 4x offered → use it (a real file beats none).
        let h = host(&[("4x.webp", "WEBP")]);
        assert_eq!(
            best_image_url(&h, true).as_deref(),
            Some("https://cdn.7tv.app/emote/abc/4x.webp")
        );
    }

    #[test]
    fn best_image_url_none_without_renderable_format() {
        let h = host(&[("1x.avif", "AVIF")]);
        assert_eq!(best_image_url(&h, false), None);
        assert_eq!(best_image_url(&h, true), None);
    }

    #[test]
    fn provider_targets_platform_specific_user_url() {
        assert_eq!(SeventvProvider::new().user_url, TWITCH_USER_URL);
        assert_eq!(SeventvProvider::for_kick().user_url, KICK_USER_URL);
    }

    #[test]
    fn parses_emote_set_and_skips_dataless() {
        let _g = scale_guard();
        bks_core::set_preferred_scale(1);
        let json = r#"{
            "emotes": [
                {"id":"1","name":"qaixxAim","data":{"animated":true,"owner":{"display_name":"Alice"},"host":{"url":"//cdn.7tv.app/emote/1","files":[{"name":"2x.gif","format":"GIF"},{"name":"2x.webp","format":"WEBP"}]}}},
                {"id":"2","name":"Broken"}
            ]
        }"#;
        let set: EmoteSet = serde_json::from_str(json).unwrap();
        let emotes = collect(set.emotes);
        assert_eq!(emotes.len(), 1);
        assert_eq!(emotes[0].name, "qaixxAim");
        assert!(emotes[0].animated);
        // Animated, but WEBP is preferred over GIF now (smaller); only 2x offered.
        assert_eq!(emotes[0].url, "https://cdn.7tv.app/emote/1/2x.webp");
        // Tooltip facts: 7TV provider + the owner's display name as author.
        assert_eq!(emotes[0].tooltip.provider, "7TV");
        assert_eq!(emotes[0].tooltip.author.as_deref(), Some("Alice"));
    }

    #[test]
    fn owner_falls_back_to_username_then_none() {
        let only_username = Owner {
            display_name: String::new(),
            username: "bob".into(),
        };
        assert_eq!(only_username.name().as_deref(), Some("bob"));
        let display = Owner {
            display_name: "Bob".into(),
            username: "bob".into(),
        };
        assert_eq!(display.name().as_deref(), Some("Bob"));
        let empty = Owner {
            display_name: String::new(),
            username: String::new(),
        };
        assert_eq!(empty.name(), None);
    }
}
