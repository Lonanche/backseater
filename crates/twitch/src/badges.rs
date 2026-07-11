//! Twitch chat badge images, fetched without authentication.
//!
//! Anonymous web viewers see badges, so the data is public: the website's own
//! GraphQL endpoint (`gql.twitch.tv`, public web client-id) returns every badge
//! for a channel — global (staff/turbo/...) and channel-specific (subscriber
//! tiers, VIP, moderator, ...) — each with its image URLs. The IRC `badges` tag
//! identifies a user's badges as `set-id/version` (e.g. `subscriber/6`), which
//! we key this map on.

use std::collections::HashMap;

use serde::Deserialize;

use crate::http::{GQL_URL, WEB_CLIENT_ID};

/// Persisted-query hash for `ChatList_Badges` (returns global + channel badges).
const BADGES_QUERY_HASH: &str = "86f43113c04606e6476e39dcd432dee47c994d77a83e54b732e11d4935f0cd08";

/// One resolved badge: its image URL and human-readable title (e.g. "Subscriber",
/// "Moderator", "VIP"), the latter shown as the hover tooltip.
#[derive(Debug, Clone)]
struct BadgeInfo {
    url: String,
    title: String,
}

/// Maps `"set-id/version"` (as it appears in the IRC `badges` tag) to a badge's
/// image URL and title.
#[derive(Debug, Default, Clone)]
pub struct BadgeMap(HashMap<String, BadgeInfo>);

impl BadgeMap {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The image URL for an IRC badge id like `"subscriber/6"`, if known.
    ///
    /// Subscriber badges fall back to the nearest lower tier: IRC sends the
    /// chatter's exact month count (e.g. `subscriber/117`), but the channel only
    /// defines images at tier thresholds (…/48, /60, /72), so an exact lookup
    /// often misses for long-time subs. We then use the highest `subscriber/M`
    /// the channel has with `M <= N` — what Twitch itself renders.
    pub fn url(&self, set_and_version: &str) -> Option<&str> {
        self.lookup(set_and_version).map(|b| b.url.as_str())
    }

    /// The title for an IRC badge id like `"subscriber/6"`, if known. Uses the
    /// same exact-then-nearest-lower-tier resolution as [`url`](Self::url) so a
    /// fallen-back subscriber badge shows the matching tier's title.
    pub fn title(&self, set_and_version: &str) -> Option<&str> {
        self.lookup(set_and_version).map(|b| b.title.as_str())
    }

    /// Resolves a badge id to its info: exact match, else (for subscribers) the
    /// nearest lower defined tier. The single resolution path behind `url`/`title`.
    fn lookup(&self, set_and_version: &str) -> Option<&BadgeInfo> {
        if let Some(info) = self.0.get(set_and_version) {
            return Some(info);
        }
        let months = set_and_version
            .strip_prefix("subscriber/")
            .and_then(|v| v.parse::<u64>().ok())?;
        self.best_subscriber_tier(months)
    }

    /// The info for the highest defined `subscriber/M` tier with `M <= months`.
    fn best_subscriber_tier(&self, months: u64) -> Option<&BadgeInfo> {
        self.0
            .iter()
            .filter_map(|(key, info)| {
                let tier = key.strip_prefix("subscriber/")?.parse::<u64>().ok()?;
                (tier <= months).then_some((tier, info))
            })
            .max_by_key(|(tier, _)| *tier)
            .map(|(_, info)| info)
    }

    /// Inserts each badge keyed on `set/version`, picking the image size for the
    /// display DPI (1x at 100% scaling, 2x above — matches the emote
    /// sizing), falling back to the other size, and skipping any without an image.
    /// Later calls overwrite earlier keys, so extending with channel badges after
    /// globals lets the channel set win.
    fn extend(&mut self, badges: Vec<GqlBadge>) {
        let prefer_2x = bks_core::preferred_scale() >= 2;
        for badge in badges {
            let (first, second) = if prefer_2x {
                (badge.image_2x, badge.image_1x)
            } else {
                (badge.image_1x, badge.image_2x)
            };
            if let Some(url) = first.or(second) {
                let title = badge.title.unwrap_or_default();
                self.0.insert(
                    format!("{}/{}", badge.set_id, badge.version),
                    BadgeInfo { url, title },
                );
            }
        }
    }
}

#[derive(Deserialize)]
struct GqlResponse {
    data: GqlData,
}

#[derive(Deserialize)]
struct GqlData {
    /// Global badges (staff/turbo/generic subscriber 0–6/...).
    #[serde(default)]
    badges: Vec<GqlBadge>,
    /// The channel, carrying its custom badges (subscriber tiers, VIP, ...).
    #[serde(default)]
    user: Option<GqlUser>,
}

#[derive(Deserialize)]
struct GqlUser {
    /// Channel-specific badges — these override globals of the same set/version.
    #[serde(rename = "broadcastBadges", default)]
    broadcast_badges: Vec<GqlBadge>,
}

#[derive(Deserialize)]
struct GqlBadge {
    #[serde(rename = "setID")]
    set_id: String,
    version: String,
    /// Human-readable name ("Subscriber", "Moderator", "VIP", ...) for the tooltip.
    #[serde(default)]
    title: Option<String>,
    /// 2x image, used at >100% DPI (see [`BadgeMap::extend`]).
    #[serde(rename = "image2x")]
    image_2x: Option<String>,
    #[serde(rename = "image1x")]
    image_1x: Option<String>,
}

/// Fetches the global + channel badge map for `channel_login` (no auth).
///
/// The response splits badges in two: `data.badges` (global: staff, turbo, the
/// generic subscriber 0–6, ...) and `data.user.broadcastBadges` (the channel's
/// own subscriber tiers, VIP, etc.). We merge both, letting the channel set
/// override globals of the same `set/version`.
pub async fn fetch_badges(channel_login: &str) -> anyhow::Result<BadgeMap> {
    let body = serde_json::json!([{
        "operationName": "ChatList_Badges",
        "variables": { "channelLogin": channel_login },
        "extensions": {
            "persistedQuery": { "version": 1, "sha256Hash": BADGES_QUERY_HASH }
        }
    }]);

    // The endpoint returns a JSON array (batched query); take the first response.
    let responses: Vec<GqlResponse> = crate::http::client()
        .post(GQL_URL)
        .header("Client-Id", WEB_CLIENT_ID)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let mut map = BadgeMap::default();
    for resp in responses {
        // Globals first, then channel badges so the channel's win on collision.
        map.extend(resp.data.badges);
        if let Some(user) = resp.data.user {
            map.extend(user.broadcast_badges);
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> BadgeMap {
        let responses: Vec<GqlResponse> = serde_json::from_str(json).unwrap();
        let mut map = BadgeMap::default();
        for resp in responses {
            map.extend(resp.data.badges);
            if let Some(user) = resp.data.user {
                map.extend(user.broadcast_badges);
            }
        }
        map
    }

    #[test]
    fn picks_size_by_dpi_and_falls_back() {
        let json = r#"[{"data":{"badges":[
            {"setID":"subscriber","version":"6","image1x":"a1","image2x":"a2"},
            {"setID":"vip","version":"1","image1x":"v1","image2x":null}
        ]}}]"#;
        // 100% DPI → prefer 1x.
        bks_core::set_preferred_scale(1);
        let map = parse(json);
        assert_eq!(map.url("subscriber/6"), Some("a1"));
        assert_eq!(map.url("vip/1"), Some("v1"));
        // HiDPI → prefer 2x, falling back to 1x when 2x is absent.
        bks_core::set_preferred_scale(2);
        let map = parse(json);
        assert_eq!(map.url("subscriber/6"), Some("a2"));
        assert_eq!(map.url("vip/1"), Some("v1")); // no 2x → 1x
        bks_core::set_preferred_scale(1); // restore default
        assert_eq!(map.url("moderator/1"), None);
    }

    #[test]
    fn channel_broadcast_badges_are_included_and_override_globals() {
        // Mirrors the real response: generic subscriber/6 in global `badges`,
        // the channel's own subscriber/6 + high tiers in `user.broadcastBadges`.
        let map = parse(
            r#"[{"data":{
                "badges":[{"setID":"subscriber","version":"6","image2x":"global6"}],
                "user":{"broadcastBadges":[
                    {"setID":"subscriber","version":"6","image2x":"chan6"},
                    {"setID":"subscriber","version":"3120","image2x":"chan3120"}
                ]}
            }}]"#,
        );
        // High channel tier resolves (the original bug).
        assert_eq!(map.url("subscriber/3120"), Some("chan3120"));
        // Channel badge overrides the global of the same set/version.
        assert_eq!(map.url("subscriber/6"), Some("chan6"));
    }

    #[test]
    fn subscriber_falls_back_to_nearest_lower_tier() {
        // Channel defines tier images at 0, 12, 72; IRC sends exact month counts.
        let map = parse(
            r#"[{"data":{"badges":[
                {"setID":"subscriber","version":"0","image2x":"t0"},
                {"setID":"subscriber","version":"12","image2x":"t12"},
                {"setID":"subscriber","version":"72","image2x":"t72"}
            ]}}]"#,
        );
        assert_eq!(map.url("subscriber/72"), Some("t72")); // exact
        assert_eq!(map.url("subscriber/117"), Some("t72")); // → highest ≤ 117
        assert_eq!(map.url("subscriber/24"), Some("t12")); // → 12, not 72
        assert_eq!(map.url("subscriber/5"), Some("t0")); // → 0
                                                         // A non-subscriber miss still returns None (no fallback).
        assert_eq!(map.url("bits/9999"), None);
    }

    #[test]
    fn title_is_parsed_and_follows_subscriber_tier_fallback() {
        let map = parse(
            r#"[{"data":{"badges":[
                {"setID":"moderator","version":"1","title":"Moderator","image2x":"m"},
                {"setID":"subscriber","version":"0","title":"Subscriber","image2x":"s0"},
                {"setID":"subscriber","version":"12","title":"1-Year Subscriber","image2x":"s12"}
            ]}}]"#,
        );
        assert_eq!(map.title("moderator/1"), Some("Moderator"));
        assert_eq!(map.title("subscriber/12"), Some("1-Year Subscriber")); // exact
        assert_eq!(map.title("subscriber/30"), Some("1-Year Subscriber")); // → ≤ 30
        assert_eq!(map.title("subscriber/3"), Some("Subscriber")); // → tier 0
        assert_eq!(map.title("vip/1"), None);
    }
}
