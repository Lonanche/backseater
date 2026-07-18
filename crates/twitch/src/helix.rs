//! Minimal Twitch Helix REST client for moderation.
//!
//! Twitch removed `/ban`, `/timeout`, and delete from IRC (Feb 2023), so these
//! go through Helix with the logged-in user's OAuth token + client id. Each call
//! needs numeric user ids (broadcaster, moderator, target), which we resolve
//! from logins via `GET /helix/users`.

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use bks_core::Emote;
use serde::Deserialize;

const HELIX: &str = "https://api.twitch.tv/helix";
/// Twitch emote CDN template; `{id}` is the emote id. Matches the size used for
/// native emotes elsewhere (dark theme, 2x).
pub(crate) const EMOTE_CDN: &str = "https://static-cdn.jtvnw.net/emoticons/v2";

/// An authenticated Helix client scoped to the logged-in moderator.
pub struct Helix {
    client: reqwest::Client,
    client_id: String,
    access_token: String,
    /// The logged-in user's id — the `moderator_id` for moderation calls.
    moderator_id: String,
    /// Resolved login → numeric id, cached because ids never change and every
    /// moderation/pin action re-resolves the (fixed) broadcaster login — without
    /// this each action paid an extra `/users` round trip. Sync mutex: never
    /// held across an await.
    user_ids: std::sync::Mutex<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct UsersResponse {
    data: Vec<HelixUser>,
}

#[derive(Deserialize)]
struct UserEmotesResponse {
    data: Vec<HelixUserEmote>,
    #[serde(default)]
    pagination: Pagination,
}

#[derive(Default, Deserialize)]
struct Pagination {
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct HelixUserEmote {
    id: String,
    name: String,
    /// Available render formats (`"static"`, `"animated"`); used to pick the
    /// CDN path and flag the emote as animated.
    #[serde(default)]
    format: Vec<String>,
}

#[derive(Deserialize)]
struct HelixUser {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    profile_image_url: String,
    #[serde(default)]
    created_at: String,
}

/// Public account info for the usercard: who they are and when the account was
/// made. `created_at` is the raw RFC-3339 string Helix returns.
#[derive(Clone, Debug)]
pub struct UserInfo {
    pub id: String,
    pub display_name: String,
    pub profile_image_url: String,
    pub created_at: String,
}

impl Helix {
    pub fn new(client_id: String, access_token: String, moderator_id: String) -> Self {
        Self {
            client: crate::http::client(),
            client_id,
            access_token,
            moderator_id,
            user_ids: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// An authed request builder — the one place the Helix auth headers live.
    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header("Client-Id", &self.client_id)
            .bearer_auth(&self.access_token)
    }

    fn get(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, url)
    }

    fn post(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, url)
    }

    fn put(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::PUT, url)
    }

    fn delete(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::DELETE, url)
    }

    fn patch(&self, url: String) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::PATCH, url)
    }

    /// Resolves a login to its numeric user id, cached after the first lookup
    /// (ids never change).
    pub async fn user_id(&self, login: &str) -> anyhow::Result<String> {
        let login = bks_core::channel_login(login);
        if let Some(id) = self.user_ids.lock().unwrap().get(&login) {
            return Ok(id.clone());
        }
        // `user_info` populates the cache on success.
        Ok(self.user_info(&login).await?.id)
    }

    /// Resolves a `(broadcaster, target)` login pair to ids concurrently — every
    /// moderation action needs both, and the two lookups are independent.
    async fn resolve_pair(&self, broadcaster: &str, target: &str) -> anyhow::Result<(String, String)> {
        let (b, t) = tokio::join!(self.user_id(broadcaster), self.user_id(target));
        Ok((b?, t?))
    }

    /// The logged-in user's own id (the `user_id`/`moderator_id` every call
    /// acts as).
    pub fn own_user_id(&self) -> &str {
        &self.moderator_id
    }

    /// Fetches public account info (id, display name, avatar, creation date) for
    /// the usercard. Works with the logged-in user's token (no extra scope).
    pub async fn user_info(&self, login: &str) -> anyhow::Result<UserInfo> {
        let login = bks_core::channel_login(login);
        let resp: UsersResponse = self
            .get(format!("{HELIX}/users"))
            .query(&[("login", login.as_str())])
            .send()
            .await
            .context("looking up user")?
            .error_for_status()?
            .json()
            .await?;
        let info = resp
            .data
            .into_iter()
            .next()
            .map(|u| UserInfo {
                id: u.id,
                display_name: u.display_name,
                profile_image_url: u.profile_image_url,
                created_at: u.created_at,
            })
            .ok_or_else(|| anyhow!("no such user '{login}'"))?;
        self.user_ids
            .lock()
            .unwrap()
            .insert(login, info.id.clone());
        Ok(info)
    }

    /// Fetches the emotes the logged-in user can use (channel subs, follower
    /// emotes, global Twitch emotes, ...), following pagination. Each becomes a
    /// renderable [`Emote`] with a CDN url; animated ones are flagged so the
    /// picker (and rendering) can pick the right variant. Passing the viewed
    /// channel's `broadcaster_id` guarantees that channel's follower emotes are
    /// included (Twitch only promises them with the param).
    pub async fn user_emotes(&self, broadcaster_id: Option<&str>) -> anyhow::Result<Vec<Emote>> {
        let mut url = format!("{HELIX}/chat/emotes/user?user_id={}", self.moderator_id);
        if let Some(b) = broadcaster_id {
            url.push_str(&format!("&broadcaster_id={b}"));
        }
        self.emote_pages(url, "listing user emotes").await
    }

    /// The logged-in user's *global* personal emote set (`user_emotes(None)` —
    /// no `broadcaster_id`), fetched **once per app** and shared across every
    /// tab. This listing is identical for every tab (it's the account's own
    /// usable emotes, independent of the viewed channel) and paginates over many
    /// pages for an emote-rich account — one fetch per tab at launch bursts
    /// hundreds of Helix calls → 429. The cache is keyed by the logged-in
    /// `user_id`; a per-key `tokio::Mutex` coalesces the launch burst onto a
    /// single fetch (later tabs await the same result rather than racing their
    /// own). A failed fetch is not cached, so it retries on the next open.
    pub async fn personal_user_emotes(&self) -> anyhow::Result<std::sync::Arc<Vec<Emote>>> {
        use std::collections::HashMap;
        use std::sync::{Arc, OnceLock};
        use tokio::sync::Mutex;

        // Per-user entry: the shared fetch lock guards both the in-flight fetch
        // and the cached result, so concurrent callers coalesce onto one request.
        type Cache = Mutex<HashMap<String, Arc<Mutex<Option<Arc<Vec<Emote>>>>>>>;
        static CACHE: OnceLock<Cache> = OnceLock::new();

        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let entry = {
            let mut map = cache.lock().await;
            map.entry(self.moderator_id.clone())
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };

        let mut guard = entry.lock().await;
        if let Some(cached) = guard.as_ref() {
            return Ok(cached.clone());
        }
        let emotes = Arc::new(self.user_emotes(None).await?);
        *guard = Some(emotes.clone());
        Ok(emotes)
    }

    /// Fetches a channel's own emote set (sub tiers, follower, bits) by numeric
    /// `broadcaster_id`, following pagination. Unlike [`user_emotes`](Self::user_emotes)
    /// this is not scoped to what the caller can *use* — it lists the channel's
    /// emotes regardless of subscription, so the picker can show them like Twitch
    /// web does (locked or not). Shares the `HelixUserEmote` shape (`GET
    /// /chat/emotes` returns the same fields).
    pub async fn channel_emotes(&self, broadcaster_id: &str) -> anyhow::Result<Vec<Emote>> {
        self.emote_pages(
            format!("{HELIX}/chat/emotes?broadcaster_id={broadcaster_id}"),
            "listing channel emotes",
        )
        .await
    }

    /// Follows a paginated Helix emote listing (`base_url` already carries its
    /// query string) and maps every page's entries to renderable [`Emote`]s —
    /// the shared body of [`user_emotes`](Self::user_emotes) and
    /// [`channel_emotes`](Self::channel_emotes), which return the same shape.
    async fn emote_pages(&self, base_url: String, what: &'static str) -> anyhow::Result<Vec<Emote>> {
        let mut emotes = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut url = base_url.clone();
            if let Some(c) = &cursor {
                // The cursor is an opaque string; percent-encode it so a '+' or
                // '/' inside it can't be mangled in the query.
                url.push_str(&format!("&after={}", bks_core::encode_url_component(c)));
            }
            let resp: UserEmotesResponse = self
                .get(url)
                .send()
                .await
                .context(what)?
                .error_for_status()?
                .json()
                .await?;
            for e in resp.data {
                let animated = e.format.iter().any(|f| f == "animated");
                let kind = if animated { "animated" } else { "static" };
                emotes.push(Emote {
                    url: format!("{EMOTE_CDN}/{}/{kind}/dark/2.0", e.id),
                    id: e.id,
                    name: e.name,
                    animated,
                    tooltip: bks_core::EmoteTooltip::provider("Twitch"),
                });
            }
            cursor = resp.pagination.cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(emotes)
    }

    /// Bans (`duration` = `None`) or times out (`Some(secs)`) `target` in
    /// `broadcaster`. Both are logins, resolved to ids here.
    pub async fn ban(
        &self,
        broadcaster: &str,
        target: &str,
        duration: Option<u32>,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;

        let mut data = HashMap::new();
        data.insert("user_id", serde_json::Value::String(target_id));
        if let Some(secs) = duration {
            data.insert("duration", serde_json::Value::from(secs));
        }
        if let Some(reason) = reason {
            data.insert("reason", serde_json::Value::String(reason.to_string()));
        }
        let body = serde_json::json!({ "data": data });

        let url = format!(
            "{HELIX}/moderation/bans?broadcaster_id={broadcaster_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self
            .post(url)
            .json(&body)
            .send()
            .await
            .context("ban request")?;
        ensure_ok(resp, "ban").await
    }

    /// Unbans (or removes a timeout for) `target` in `broadcaster`. Both are
    /// logins, resolved to ids here.
    pub async fn unban(&self, broadcaster: &str, target: &str) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;
        let url = format!(
            "{HELIX}/moderation/bans?broadcaster_id={broadcaster_id}\
             &moderator_id={}&user_id={target_id}",
            self.moderator_id
        );
        let resp = self.delete(url).send().await.context("unban request")?;
        ensure_ok(resp, "unban").await
    }

    /// Grants moderator to `target` in `broadcaster` (broadcaster token only —
    /// only the channel owner can add/remove mods, so this 401/403s for a regular
    /// mod, surfaced as the error body). Both args are logins, resolved to ids.
    pub async fn add_moderator(&self, broadcaster: &str, target: &str) -> anyhow::Result<()> {
        self.set_role("moderation/moderators", true, broadcaster, target, "add mod")
            .await
    }

    /// Revokes moderator from `target` in `broadcaster` (broadcaster token only).
    pub async fn remove_moderator(&self, broadcaster: &str, target: &str) -> anyhow::Result<()> {
        self.set_role(
            "moderation/moderators",
            false,
            broadcaster,
            target,
            "remove mod",
        )
        .await
    }

    /// Grants VIP to `target` in `broadcaster` (broadcaster token only).
    pub async fn add_vip(&self, broadcaster: &str, target: &str) -> anyhow::Result<()> {
        self.set_role("channels/vips", true, broadcaster, target, "add VIP")
            .await
    }

    /// Revokes VIP from `target` in `broadcaster` (broadcaster token only).
    pub async fn remove_vip(&self, broadcaster: &str, target: &str) -> anyhow::Result<()> {
        self.set_role("channels/vips", false, broadcaster, target, "remove VIP")
            .await
    }

    /// Grants (`POST`) or revokes (`DELETE`) a role at `endpoint` (moderators /
    /// VIPs share the same query shape) — the body of the four role methods.
    async fn set_role(
        &self,
        endpoint: &str,
        grant: bool,
        broadcaster: &str,
        target: &str,
        action: &str,
    ) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;
        let url = format!("{HELIX}/{endpoint}?broadcaster_id={broadcaster_id}&user_id={target_id}");
        let method = if grant {
            reqwest::Method::POST
        } else {
            reqwest::Method::DELETE
        };
        let resp = self
            .request(method, url)
            .send()
            .await
            .with_context(|| format!("{action} request"))?;
        ensure_ok(resp, action).await
    }

    /// Allows or denies a message AutoMod is holding for review. `message_id`
    /// comes from the EventSub `automod.message.hold` notification; the caller
    /// must moderate the channel it was held in (`moderator:manage:automod`).
    pub async fn manage_automod_message(
        &self,
        message_id: &str,
        allow: bool,
    ) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "user_id": self.moderator_id,
            "msg_id": message_id,
            "action": if allow { "ALLOW" } else { "DENY" },
        });
        let url = format!("{HELIX}/moderation/automod/message");
        let resp = self
            .post(url)
            .json(&body)
            .send()
            .await
            .context("automod request")?;
        ensure_ok(
            resp,
            if allow {
                "automod allow"
            } else {
                "automod deny"
            },
        )
        .await
    }

    /// Sends `text` to `broadcaster`'s chat as the logged-in user via Helix
    /// (`user:write:chat`), returning the new message's id — the send path for
    /// `/pin`, which needs the real id to pin (IRC gives a sent message no id;
    /// the message still arrives in chat through the read connection as a
    /// normal PRIVMSG).
    pub async fn send_message(&self, broadcaster: &str, text: &str) -> anyhow::Result<String> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let body = serde_json::json!({
            "broadcaster_id": broadcaster_id,
            "sender_id": self.moderator_id,
            "message": text,
        });
        let url = format!("{HELIX}/chat/messages");
        let resp = self
            .post(url)
            .json(&body)
            .send()
            .await
            .context("send request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("send failed ({status}): {body}");
        }
        let resp: SendMessageResponse = resp.json().await.context("send response")?;
        let sent = resp
            .data
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("send returned no message"))?;
        if !sent.is_sent {
            let reason = sent
                .drop_reason
                .map(|r| r.message)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "message dropped".into());
            anyhow::bail!("{reason}");
        }
        Ok(sent.message_id)
    }

    /// Pins the message with `message_id` in `broadcaster`'s chat for
    /// `duration_secs` (Twitch clamps to 30–1800; `None` pins until the stream
    /// ends). One mod pin is active per channel — pinning replaces the current one.
    pub async fn pin_message(
        &self,
        broadcaster: &str,
        message_id: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<()> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let mut url = format!(
            "{HELIX}/chat/pins?broadcaster_id={broadcaster_id}\
             &moderator_id={}&message_id={message_id}",
            self.moderator_id
        );
        if let Some(secs) = duration_secs {
            url.push_str(&format!("&duration_seconds={secs}"));
        }
        let resp = self.put(url).send().await.context("pin request")?;
        ensure_ok(resp, "pin").await
    }

    /// Unpins the pinned message with `message_id` in `broadcaster`'s chat.
    pub async fn unpin_message(&self, broadcaster: &str, message_id: &str) -> anyhow::Result<()> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let url = format!(
            "{HELIX}/chat/pins?broadcaster_id={broadcaster_id}\
             &moderator_id={}&message_id={message_id}",
            self.moderator_id
        );
        let resp = self.delete(url).send().await.context("unpin request")?;
        ensure_ok(resp, "unpin").await
    }

    /// The channel's currently pinned message, if any — used to seed the pinned
    /// banner on join. Requires the caller to moderate `broadcaster_id`'s channel
    /// (`moderator:manage:chat_messages` covers the read); a 403 just means "not
    /// a mod there", which callers treat as "no seed".
    pub async fn pinned_message(&self, broadcaster_id: &str) -> anyhow::Result<Option<PinnedChat>> {
        let url = format!(
            "{HELIX}/chat/pins?broadcaster_id={broadcaster_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self
            .get(url)
            .send()
            .await
            .context("pinned-message lookup")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("pinned-message lookup failed ({status}): {body}");
        }
        let resp: PinsResponse = resp.json().await?;
        Ok(resp.data.into_iter().next())
    }

    /// The users currently connected to `broadcaster`'s chat, via
    /// `GET /chat/chatters` (paginated, 1000 per page). Twitch only exposes this
    /// to the broadcaster and their moderators (`moderator:read:chatters`) — the
    /// anonymous list the website shows rides its browser-integrity-gated GQL,
    /// which third-party clients can't use. Common failures are rephrased: a 403
    /// means "not a mod there", a 401 usually means a token from before the
    /// chatters scope was requested.
    pub async fn chatters(&self, broadcaster: &str) -> anyhow::Result<Chatters> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let mut chatters = Vec::new();
        let mut total: Option<u64> = None;
        let mut cursor: Option<String> = None;
        loop {
            let mut url = format!(
                "{HELIX}/chat/chatters?broadcaster_id={broadcaster_id}\
                 &moderator_id={}&first=1000",
                self.moderator_id
            );
            if let Some(c) = &cursor {
                url.push_str(&format!("&after={c}"));
            }
            let resp = self.get(url).send().await.context("chatters request")?;
            let status = resp.status();
            if status == reqwest::StatusCode::FORBIDDEN {
                anyhow::bail!(
                    "Twitch only shows the viewer list to the broadcaster and \
                     moderators of the channel"
                );
            }
            if status == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!(
                    "Twitch rejected the request (401) — if you logged in before \
                     the viewer list was added, log out and back in (Settings → \
                     Account) to grant it"
                );
            }
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("chatters lookup failed ({status}): {body}");
            }
            let page: ChattersResponse = resp.json().await?;
            total.get_or_insert(page.total);
            chatters.extend(page.data);
            cursor = page.pagination.cursor;
            if cursor.is_none() {
                break;
            }
        }
        Ok(Chatters {
            total: total.unwrap_or(chatters.len() as u64),
            chatters,
        })
    }

    /// Deletes a single message by id in `broadcaster`.
    pub async fn delete_message(&self, broadcaster: &str, message_id: &str) -> anyhow::Result<()> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let url = format!(
            "{HELIX}/moderation/chat?broadcaster_id={broadcaster_id}\
             &moderator_id={}&message_id={message_id}",
            self.moderator_id
        );
        let resp = self.delete(url).send().await.context("delete request")?;
        ensure_ok(resp, "delete").await
    }

    /// Clears `broadcaster`'s entire chat (the same endpoint as
    /// [`delete_message`](Self::delete_message), minus the message id).
    pub async fn clear_chat(&self, broadcaster: &str) -> anyhow::Result<()> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let url = format!(
            "{HELIX}/moderation/chat?broadcaster_id={broadcaster_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self.delete(url).send().await.context("clear request")?;
        ensure_ok(resp, "clear").await
    }

    /// Posts an announcement in `broadcaster`'s chat. `color` is one of
    /// blue/green/orange/purple (`None` = the channel's accent color). Needs
    /// `moderator:manage:announcements`.
    pub async fn announce(
        &self,
        broadcaster: &str,
        message: &str,
        color: Option<&str>,
    ) -> anyhow::Result<()> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let mut body = serde_json::json!({ "message": message });
        if let Some(color) = color {
            body["color"] = serde_json::Value::String(color.to_string());
        }
        let url = format!(
            "{HELIX}/chat/announcements?broadcaster_id={broadcaster_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self
            .post(url)
            .json(&body)
            .send()
            .await
            .context("announce request")?;
        ensure_ok(resp, "announce").await
    }

    /// Warns `target` in `broadcaster`'s chat with `reason` — the user must
    /// acknowledge it before chatting again. Needs `moderator:manage:warnings`.
    pub async fn warn(&self, broadcaster: &str, target: &str, reason: &str) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;
        let body = serde_json::json!({ "data": { "user_id": target_id, "reason": reason } });
        let url = format!(
            "{HELIX}/moderation/warnings?broadcaster_id={broadcaster_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self
            .post(url)
            .json(&body)
            .send()
            .await
            .context("warn request")?;
        ensure_ok(resp, "warn").await
    }

    /// Marks `target` as a suspicious user in `broadcaster`'s chat — restricted
    /// (messages withheld from chat, delivered to mods for review) or merely
    /// monitored. Needs `moderator:manage:suspicious_users`.
    pub async fn add_suspicious_user(
        &self,
        broadcaster: &str,
        target: &str,
        restricted: bool,
    ) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;
        let status = if restricted {
            "RESTRICTED"
        } else {
            "ACTIVE_MONITORING"
        };
        let body = serde_json::json!({ "user_id": target_id, "status": status });
        let url = format!(
            "{HELIX}/moderation/suspicious_users?broadcaster_id={broadcaster_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self
            .post(url)
            .json(&body)
            .send()
            .await
            .context("suspicious-user request")?;
        ensure_ok(resp, "suspicious user").await
    }

    /// Removes `target`'s suspicious-user treatment in `broadcaster`'s chat.
    /// Needs `moderator:manage:suspicious_users`.
    pub async fn remove_suspicious_user(
        &self,
        broadcaster: &str,
        target: &str,
    ) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;
        let url = format!(
            "{HELIX}/moderation/suspicious_users?broadcaster_id={broadcaster_id}\
             &moderator_id={}&user_id={target_id}",
            self.moderator_id
        );
        let resp = self
            .delete(url)
            .send()
            .await
            .context("suspicious-user request")?;
        ensure_ok(resp, "suspicious user").await
    }

    /// Patches `broadcaster`'s chat settings with the given (partial) `settings`
    /// body — only the fields present change. Needs
    /// `moderator:manage:chat_settings`. The typed toggles live on
    /// [`crate::TwitchActions`].
    pub async fn update_chat_settings(
        &self,
        broadcaster: &str,
        settings: serde_json::Value,
    ) -> anyhow::Result<()> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let url = format!(
            "{HELIX}/chat/settings?broadcaster_id={broadcaster_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self
            .patch(url)
            .json(&settings)
            .send()
            .await
            .context("chat-settings request")?;
        ensure_ok(resp, "chat settings").await
    }

    /// Sends an official shoutout for `target` in `broadcaster`'s chat. Needs
    /// `moderator:manage:shoutouts`; Twitch rate-limits these (surfaced as the
    /// error body).
    pub async fn shoutout(&self, broadcaster: &str, target: &str) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;
        let url = format!(
            "{HELIX}/chat/shoutouts?from_broadcaster_id={broadcaster_id}\
             &to_broadcaster_id={target_id}&moderator_id={}",
            self.moderator_id
        );
        let resp = self.post(url).send().await.context("shoutout request")?;
        ensure_ok(resp, "shoutout").await
    }

    /// Starts a raid from `broadcaster` to `target` (broadcaster token only —
    /// `channel:manage:raids`).
    pub async fn raid(&self, broadcaster: &str, target: &str) -> anyhow::Result<()> {
        let (broadcaster_id, target_id) = self.resolve_pair(broadcaster, target).await?;
        let url = format!(
            "{HELIX}/raids?from_broadcaster_id={broadcaster_id}&to_broadcaster_id={target_id}"
        );
        let resp = self.post(url).send().await.context("raid request")?;
        ensure_ok(resp, "raid").await
    }

    /// Cancels `broadcaster`'s pending raid (broadcaster token only).
    pub async fn cancel_raid(&self, broadcaster: &str) -> anyhow::Result<()> {
        let broadcaster_id = self.user_id(broadcaster).await?;
        let url = format!("{HELIX}/raids?broadcaster_id={broadcaster_id}");
        let resp = self.delete(url).send().await.context("unraid request")?;
        ensure_ok(resp, "unraid").await
    }
}

#[derive(Deserialize)]
struct ChattersResponse {
    #[serde(default)]
    data: Vec<Chatter>,
    #[serde(default)]
    pagination: Pagination,
    #[serde(default)]
    total: u64,
}

/// One user connected to a channel's chat, from `GET /chat/chatters`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Chatter {
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub user_login: String,
    #[serde(default)]
    pub user_name: String,
}

/// The full viewer list of a channel: every page of chatters plus Twitch's
/// total count (they match unless the list changed mid-pagination).
#[derive(Clone, Debug, Default)]
pub struct Chatters {
    pub total: u64,
    pub chatters: Vec<Chatter>,
}

#[derive(Deserialize)]
struct SendMessageResponse {
    #[serde(default)]
    data: Vec<SentMessage>,
}

/// One entry of `POST /helix/chat/messages`: the sent message's id, or why it
/// was dropped (AutoMod, chat modes, ...).
#[derive(Default, Deserialize)]
struct SentMessage {
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    is_sent: bool,
    #[serde(default)]
    drop_reason: Option<DropReason>,
}

#[derive(Default, Deserialize)]
struct DropReason {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct PinsResponse {
    #[serde(default)]
    data: Vec<PinnedChat>,
}

/// One entry of `GET /helix/chat/pins`: the pinned message with its sender, who
/// pinned it, and the expiry. All fields defaulted — the endpoint is new and a
/// partial parse still seeds a usable banner.
#[derive(Default, Deserialize)]
pub struct PinnedChat {
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    sender_user_id: String,
    #[serde(default)]
    sender_user_login: String,
    #[serde(default)]
    sender_user_name: String,
    #[serde(default)]
    pub pinned_by_user_name: String,
    #[serde(default)]
    message: PinnedChatBody,
    /// RFC-3339; when the message was pinned.
    #[serde(default)]
    starts_at: String,
    /// RFC-3339; empty/null when pinned until the stream ends.
    #[serde(default)]
    ends_at: Option<String>,
}

#[derive(Default, Deserialize)]
struct PinnedChatBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    fragments: Vec<PinnedChatFragment>,
}

#[derive(Default, Deserialize)]
struct PinnedChatFragment {
    #[serde(default)]
    text: String,
    #[serde(default)]
    emote: Option<PinnedChatEmote>,
}

#[derive(Default, Deserialize)]
struct PinnedChatEmote {
    #[serde(default)]
    id: String,
}

impl PinnedChat {
    /// When the pin expires; `None` = until unpinned / stream end.
    pub fn ends_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        bks_core::parse_rfc3339(self.ends_at.as_deref().unwrap_or_default())
    }

    /// The pinned message as a renderable [`bks_core::Message`] (text + native
    /// emote fragments; no badges — the banner doesn't need them).
    pub fn to_message(&self) -> bks_core::Message {
        let mut elements: Vec<bks_core::MessageElement> = Vec::new();
        for fragment in &self.message.fragments {
            match &fragment.emote {
                Some(emote) if !emote.id.is_empty() => {
                    elements.push(bks_core::MessageElement::Emote(std::sync::Arc::new(
                        Emote {
                            url: format!("{EMOTE_CDN}/{}/default/dark/2.0", emote.id),
                            id: emote.id.clone(),
                            name: fragment.text.clone(),
                            animated: false,
                            tooltip: bks_core::EmoteTooltip::provider("Twitch"),
                        },
                    )));
                }
                _ if !fragment.text.is_empty() => {
                    elements.push(bks_core::MessageElement::Text {
                        text: fragment.text.clone(),
                        color: None,
                    });
                }
                _ => {}
            }
        }
        if elements.is_empty() && !self.message.text.is_empty() {
            elements.push(bks_core::MessageElement::Text {
                text: self.message.text.clone(),
                color: None,
            });
        }
        bks_core::Message {
            id: self.message_id.clone(),
            platform: bks_core::Platform::Twitch,
            channel: String::new(),
            // The sent time isn't in the response; the pin time is close enough
            // for the banner's timestamp.
            timestamp: bks_core::parse_rfc3339(&self.starts_at).unwrap_or_else(chrono::Utc::now),
            author: bks_core::Author {
                login: self.sender_user_login.clone(),
                display_name: if self.sender_user_name.is_empty() {
                    self.sender_user_login.clone()
                } else {
                    self.sender_user_name.clone()
                },
                color: None,
                badges: Vec::new(),
                paint: None,
                user_id: self.sender_user_id.clone(),
            },
            raw_text: self.message.text.clone(),
            elements: bks_core::mentionize(bks_core::linkify(elements)),
            reply: None,
            first_message: false,
            highlighted: false,
            historical: false,
            reward_id: None,
        }
    }
}

/// Fetches `broadcaster_id`'s currently pinned message with the logged-in
/// user's credentials (`auth`), for seeding the pinned banner on join. Returns
/// the message plus who pinned it and the expiry; `Ok(None)` when nothing is
/// pinned. Errors (including the 403 a non-moderator gets) are the caller's to
/// log-and-ignore — an unseeded banner is the normal fallback.
pub async fn fetch_pinned_message(
    auth: &crate::EventsubAuth,
    broadcaster_id: &str,
) -> anyhow::Result<
    Option<(
        bks_core::Message,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    )>,
> {
    let helix = Helix::new(
        auth.client_id.clone(),
        auth.token.clone(),
        auth.user_id.clone(),
    );
    let pin = helix.pinned_message(broadcaster_id).await?;
    Ok(pin.map(|p| {
        let ends_at = p.ends_at();
        (p.to_message(), p.pinned_by_user_name, ends_at)
    }))
}

/// Turns a non-2xx Helix response into an error carrying its body (Helix puts a
/// useful `message` field there), so failures aren't just "400 Bad Request".
async fn ensure_ok(resp: reqwest::Response, action: &str) -> anyhow::Result<()> {
    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("{action} failed ({status}): {body}"))
}
