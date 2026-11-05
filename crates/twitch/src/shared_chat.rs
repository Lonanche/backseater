//! Twitch shared chat: a message relayed from another channel in the session
//! carries that channel's id in the IRC `source-room-id` tag and the chatter's
//! badges *in that channel* in `source-badges`. The UI shows the source
//! channel's avatar before the line (like twitch.tv), and the badges must be
//! resolved against the source channel's set (its subscriber tiers differ). So
//! the id is resolved once per channel to name + avatar (IVR, no auth) + badge
//! set (GQL, no auth) and cached process-wide — a shared session has only a
//! handful of channels. Messages are never held for it: the bridge loads it in
//! the background and the UI re-renders the relayed rows when it lands.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use bks_core::{Badge, MessageSource};
use serde::Deserialize;
use tokio::sync::OnceCell;

const IVR_USER_URL: &str = "https://api.ivr.fi/v2/twitch/user";
/// A failed lookup is retried after this long instead of on every message.
const RETRY_AFTER: Duration = Duration::from_secs(300);
/// Bounds one lookup so a stalled request can't pin a channel as "loading".
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

/// The raw shared-chat IRC tags of a PRIVMSG (both absent for normal chat).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SharedChatTags<'a> {
    pub room_id: Option<&'a str>,
    pub badges: Option<&'a str>,
}

/// A resolved shared-chat source channel: what the UI draws for its relayed
/// messages. Read at render time via [`shared_source`], so messages never wait
/// for the lookup — rows re-render once it lands.
#[derive(Debug)]
pub struct SharedSource {
    pub name: String,
    pub icon_url: String,
    /// Empty when the badge fetch failed (relayed badges are then dropped
    /// rather than resolved against the wrong channel).
    badges: crate::BadgeMap,
}

impl SharedSource {
    /// Resolves a relayed message's `source-badges` against this channel's set,
    /// dropping any without a known image.
    pub fn resolve_badges(&self, badges: &[Badge]) -> Vec<Badge> {
        badges
            .iter()
            .filter_map(|badge| {
                Some(Badge {
                    id: badge.id.clone(),
                    url: self.badges.url(&badge.id)?.to_string(),
                    title: self.badges.title(&badge.id),
                })
            })
            .collect()
    }
}

type Lookup = (Option<Arc<SharedSource>>, Instant);

fn cache() -> &'static Mutex<HashMap<String, Arc<OnceCell<Lookup>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<OnceCell<Lookup>>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The [`MessageSource`] for a PRIVMSG, or `None` when it was sent in the
/// channel itself (no `source-room-id`, or it equals `room-id`).
pub(crate) fn source_from_tags(
    room_id: &str,
    source_room_id: Option<&str>,
) -> Option<MessageSource> {
    let source = source_room_id.filter(|id| !id.is_empty() && *id != room_id)?;
    Some(MessageSource {
        id: source.to_string(),
    })
}

/// Parses a `source-badges` tag (`subscriber/12,moderator/1`, same shape as
/// `badges`) into unresolved `set/version` badges.
pub(crate) fn badges_from_tag(raw: &str) -> Vec<Badge> {
    raw.split(',')
        .filter(|b| b.contains('/'))
        .map(|id| Badge {
            id: id.to_string(),
            url: String::new(),
            title: None,
        })
        .collect()
}

/// The source channel's resolved info, if a lookup has already succeeded.
/// Never blocks or fetches — cheap enough for the render path.
pub fn shared_source(id: &str) -> Option<Arc<SharedSource>> {
    let map = cache().lock().unwrap_or_else(|e| e.into_inner());
    map.get(id)?.get()?.0.clone()
}

/// Loads a source channel (name + avatar via IVR, badge set via GQL), sharing
/// one in-flight fetch between all callers. Returns whether the info is now
/// available. A failed or badge-less lookup is retried after
/// [`RETRY_AFTER`]; until then this returns the cached result immediately.
pub async fn load_shared_source(id: &str) -> bool {
    let cell = {
        let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(id.to_string()).or_default();
        if let Some((info, at)) = entry.get() {
            let incomplete = info.as_ref().is_none_or(|i| i.badges.is_empty());
            if incomplete && at.elapsed() >= RETRY_AFTER {
                *entry = Arc::default();
            }
        }
        entry.clone()
    };
    let (info, _) = cell
        .get_or_init(|| async {
            let info = match tokio::time::timeout(LOOKUP_TIMEOUT, fetch(id)).await {
                Ok(Ok(info)) => info.map(Arc::new),
                Ok(Err(err)) => {
                    tracing::debug!("shared chat source {id} lookup failed: {err:#}");
                    None
                }
                Err(_) => {
                    tracing::debug!("shared chat source {id} lookup timed out");
                    None
                }
            };
            (info, Instant::now())
        })
        .await;
    info.is_some()
}

#[derive(Deserialize)]
struct IvrUser {
    #[serde(default)]
    login: String,
    #[serde(default, rename = "displayName")]
    display_name: String,
    #[serde(default)]
    logo: String,
}

async fn fetch(id: &str) -> anyhow::Result<Option<SharedSource>> {
    let Some(user) = fetch_user(id).await? else {
        return Ok(None);
    };
    let badges = match crate::fetch_badges(&user.login).await {
        Ok(map) => map,
        Err(err) => {
            tracing::debug!("shared chat source {id} badge load failed: {err:#}");
            crate::BadgeMap::default()
        }
    };
    Ok(Some(info_from_user(user, badges)))
}

async fn fetch_user(id: &str) -> anyhow::Result<Option<IvrUser>> {
    let users: Vec<IvrUser> = crate::http::client()
        .get(IVR_USER_URL)
        .query(&[("id", id)])
        .send()
        .await
        .context("requesting IVR user")?
        .error_for_status()
        .context("IVR user request failed")?
        .json()
        .await
        .context("parsing IVR user response")?;
    Ok(users.into_iter().next().filter(|u| !u.login.is_empty()))
}

fn info_from_user(user: IvrUser, badges: crate::BadgeMap) -> SharedSource {
    let name = if user.display_name.is_empty() {
        user.login
    } else {
        user.display_name
    };
    SharedSource {
        name,
        icon_url: small_avatar(&user.logo),
        badges,
    }
}

/// Twitch avatars come as `…-profile_image-300x300.png`; the CDN also serves a
/// 70x70 variant, plenty for a badge-sized icon.
fn small_avatar(url: &str) -> String {
    url.replace("-300x300.", "-70x70.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_channel_messages_have_no_source() {
        assert_eq!(source_from_tags("123", None), None);
        assert_eq!(source_from_tags("123", Some("")), None);
        assert_eq!(source_from_tags("123", Some("123")), None);
        assert_eq!(source_from_tags("123", Some("456")).unwrap().id, "456");
    }

    #[test]
    fn ivr_user_maps_to_display_name_and_small_avatar() {
        let users: Vec<IvrUser> = serde_json::from_str(
            r#"[{"id":"456","login":"other","displayName":"Other",
                "logo":"https://static-cdn.jtvnw.net/jtv_user_pictures/abc-profile_image-300x300.png"}]"#,
        )
        .unwrap();
        let info = info_from_user(users.into_iter().next().unwrap(), Default::default());
        assert_eq!(info.name, "Other");
        assert_eq!(
            info.icon_url,
            "https://static-cdn.jtvnw.net/jtv_user_pictures/abc-profile_image-70x70.png"
        );
    }

    #[test]
    fn source_badges_tag_parses_like_badges() {
        let ids: Vec<String> = badges_from_tag("subscriber/12,moderator/1,")
            .into_iter()
            .map(|b| b.id)
            .collect();
        assert_eq!(ids, ["subscriber/12", "moderator/1"]);
    }

    #[test]
    fn relayed_message_uses_source_badges() {
        use tmi::{FromIrc, IrcMessageRef};
        let raw = "@badges=subscriber/3;source-badges=subscriber/24,vip/1;room-id=1;source-room-id=2;id=a;user-id=9;display-name=U;tmi-sent-ts=1 :u!u@u.tmi.twitch.tv PRIVMSG #one :hi";
        let irc = IrcMessageRef::parse(raw).unwrap();
        let shared = SharedChatTags {
            room_id: irc.tag("source-room-id"),
            badges: irc.tag("source-badges"),
        };
        let pm = tmi::msg::Privmsg::from_irc(irc.clone()).unwrap();
        let msg = crate::connector::privmsg_to_message("#one", &pm, false, "", shared);
        assert_eq!(msg.source.unwrap().id, "2");
        let ids: Vec<&str> = msg.author.badges.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["subscriber/24", "vip/1"]);
    }
}
