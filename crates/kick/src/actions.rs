//! Authenticated Kick actions over the public REST API: sending chat messages
//! and moderation (ban/timeout). Unlike Twitch (IRC sends on the read socket),
//! Kick sends are stateless HTTPS POSTs, independent of the Pusher read stream.
//!
//! Endpoints (all need a scoped user token):
//! - send:   `POST /public/v1/chat`               `{broadcaster_user_id, content, type}` (`chat:write`)
//! - ban:    `POST /public/v1/moderation/bans`    `{broadcaster_user_id, user_id, duration?, reason?}` (`moderation:ban`)
//! - unban:  `DELETE /public/v1/moderation/bans`
//! - delete: `DELETE /public/v1/chat/{message_id}` (`moderation:chat_message:manage`)
//!
//! Pin/unpin are intentionally **absent**: Kick's public API has no endpoints
//! for them, and the site API (`kick.com/api/v2/...`) only authenticates
//! kick.com *web session* tokens — it rejects public-API OAuth tokens
//! (verified live: 401 "Unauthenticated"; the web client's bearer is literally
//! its `session_token` cookie value). Disabled until Kick adds public
//! endpoints. (Delete used to be in that bucket too, until Kick added the
//! public endpoint above.)
//!
//! **Token refresh.** Kick access tokens are short-lived (~hours). Each authed
//! request that comes back `401` triggers one refresh through the broker
//! (`POST /kick/refresh` with the rotating refresh token), then the request is
//! retried once. A successful refresh fires the [`OnRefreshed`] callback so the
//! caller can persist the new tokens and keep the login alive; if the refresh
//! itself fails the call returns [`AuthExpired`], which the session treats as a
//! real logout (clearing the stale "logged in" UI). Without this the saved token
//! silently expired while the app kept claiming it was logged in.

use std::sync::Arc;

use anyhow::{anyhow, Context};
use serde::Deserialize;
use tokio::sync::Mutex;

const CHAT_URL: &str = "https://api.kick.com/public/v1/chat";
const BANS_URL: &str = "https://api.kick.com/public/v1/moderation/bans";
const CHANNELS_URL: &str = "https://api.kick.com/public/v1/channels";

/// The current access + refresh tokens, replaced atomically on each refresh.
#[derive(Clone)]
struct Tokens {
    access_token: String,
    refresh_token: String,
}

/// Fired after a successful token refresh with the rotated `(access, refresh)`
/// tokens, so the owner (the session) can persist them and update its snapshot.
pub type OnRefreshed = Arc<dyn Fn(String, String) + Send + Sync>;

/// Returned when a request was `401` and the refresh attempt also failed — the
/// Kick login is genuinely dead (revoked, or the refresh token expired), so the
/// caller should log out of Kick rather than keep retrying.
#[derive(Debug)]
pub struct AuthExpired;

impl std::fmt::Display for AuthExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kick login expired — please log in again (Settings → Account)")
    }
}

impl std::error::Error for AuthExpired {}

/// Authenticated Kick REST client for one logged-in user. Refreshes its own token
/// on `401` via the broker.
pub struct KickActions {
    client: reqwest::Client,
    tokens: Mutex<Tokens>,
    /// Broker base URL (no trailing slash) used to refresh the token.
    broker_url: String,
    /// Notified with the rotated tokens after a successful refresh.
    on_refreshed: Mutex<Option<OnRefreshed>>,
    /// Resolved slug → broadcaster id, cached because the mapping never changes
    /// and every send/moderation call needs it — without this each chat message
    /// cost an extra channel-lookup round trip. Sync mutex: never held across
    /// an await.
    broadcaster_ids: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

#[derive(Deserialize)]
struct ChannelsResponse {
    data: Vec<ChannelEntry>,
}

#[derive(Deserialize)]
struct ChannelEntry {
    broadcaster_user_id: u64,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: String,
}

impl KickActions {
    /// Builds a client. `refresh_token` + `broker_url` let it refresh an expired
    /// access token transparently; pass them from the saved [`Credentials`].
    pub fn new(access_token: String, refresh_token: String, broker_url: String) -> Self {
        Self {
            // Timeouts, since reqwest's default has none — a stalled connection
            // would hang a send/moderation action forever.
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("building kick http client"),
            tokens: Mutex::new(Tokens {
                access_token,
                refresh_token,
            }),
            broker_url,
            on_refreshed: Mutex::new(None),
            broadcaster_ids: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Sets the callback invoked with `(access_token, refresh_token)` after a
    /// successful refresh, so the owner can persist them.
    pub async fn set_on_refreshed(&self, cb: OnRefreshed) {
        *self.on_refreshed.lock().await = Some(cb);
    }

    /// The current access token (snapshot).
    async fn access_token(&self) -> String {
        self.tokens.lock().await.access_token.clone()
    }

    /// Runs an authed request builder, retrying once through a token refresh if it
    /// comes back `401`. `build` is called fresh for each attempt (a `RequestBuilder`
    /// can't be cloned reliably with a body) with the current access token.
    async fn send_authed(
        &self,
        action: &str,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        let token = self.access_token().await;
        let resp = build(&self.client, &token)
            .send()
            .await
            .with_context(|| format!("kick {action} request"))?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        // Expired/invalid token: refresh once and retry. A failed refresh means the
        // login is dead — surface it as `AuthExpired` so the session logs out.
        self.refresh().await.map_err(|_| anyhow!(AuthExpired))?;
        let token = self.access_token().await;
        build(&self.client, &token)
            .send()
            .await
            .with_context(|| format!("kick {action} retry"))
    }

    /// Exchanges the refresh token for a new access token via the broker, updates
    /// the in-memory tokens, and notifies the persist callback. The refresh token
    /// rotates, so the new one must be saved too.
    async fn refresh(&self) -> anyhow::Result<()> {
        let refresh_token = self.tokens.lock().await.refresh_token.clone();
        let body = serde_json::json!({ "refresh_token": refresh_token });
        let new: RefreshResponse = self
            .client
            .post(format!("{}/kick/refresh", self.broker_url))
            .json(&body)
            .send()
            .await
            .context("refreshing Kick token")?
            .error_for_status()
            .context("broker refused the refresh token")?
            .json()
            .await?;
        {
            let mut t = self.tokens.lock().await;
            t.access_token = new.access_token.clone();
            t.refresh_token = new.refresh_token.clone();
        }
        if let Some(cb) = self.on_refreshed.lock().await.as_ref() {
            cb(new.access_token, new.refresh_token);
        }
        Ok(())
    }

    /// Resolves a channel slug to its broadcaster user id (needed as
    /// `broadcaster_user_id` for send + moderation). Cached after the first
    /// lookup — the id never changes, and this runs before every send.
    pub async fn broadcaster_id(&self, slug: &str) -> anyhow::Result<u64> {
        let slug = bks_core::channel_login(slug);
        if let Some(id) = self.broadcaster_ids.lock().unwrap().get(&slug) {
            return Ok(*id);
        }
        let url = format!(
            "{CHANNELS_URL}?slug={}",
            bks_core::encode_url_component(&slug)
        );
        let resp = self
            .send_authed("channel lookup", |client, token| {
                client.get(&url).bearer_auth(token)
            })
            .await?
            .error_for_status()?;
        let resp: ChannelsResponse = resp.json().await?;
        let id = resp
            .data
            .into_iter()
            .next()
            .map(|c| c.broadcaster_user_id)
            .ok_or_else(|| anyhow!("no such Kick channel '{slug}'"))?;
        self.broadcaster_ids.lock().unwrap().insert(slug, id);
        Ok(id)
    }

    /// Sends a chat message to `broadcaster_id` as the logged-in user. When
    /// `reply_to_message_id` is set, the message threads as a reply (Kick's public
    /// chat API `reply_to_message_id` field).
    pub async fn send(
        &self,
        broadcaster_id: u64,
        text: &str,
        reply_to_message_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::json!({
            "broadcaster_user_id": broadcaster_id,
            "content": text,
            "type": "user",
        });
        if let Some(id) = reply_to_message_id {
            body["reply_to_message_id"] = serde_json::Value::String(id.to_string());
        }
        let resp = self
            .send_authed("send", |client, token| {
                client.post(CHAT_URL).bearer_auth(token).json(&body)
            })
            .await?;
        ensure_ok(resp, "send").await
    }

    /// Bans (`duration_minutes` = `None`) or times out a user in a channel.
    pub async fn ban(
        &self,
        broadcaster_id: u64,
        target_id: u64,
        duration_minutes: Option<u32>,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut body = serde_json::Map::new();
        body.insert("broadcaster_user_id".into(), broadcaster_id.into());
        body.insert("user_id".into(), target_id.into());
        if let Some(mins) = duration_minutes {
            body.insert("duration".into(), mins.into());
        }
        if let Some(reason) = reason {
            body.insert("reason".into(), reason.into());
        }
        let body = serde_json::Value::Object(body);
        let resp = self
            .send_authed("ban", |client, token| {
                client.post(BANS_URL).bearer_auth(token).json(&body)
            })
            .await?;
        ensure_ok(resp, "ban").await
    }

    /// Deletes one chat message via the public API
    /// (`DELETE /public/v1/chat/{message_id}`, scope
    /// `moderation:chat_message:manage`). Keys on the message id alone — no
    /// broadcaster/chatroom id needed.
    pub async fn delete_message(&self, message_id: &str) -> anyhow::Result<()> {
        let url = format!(
            "{CHAT_URL}/{}",
            bks_core::encode_url_component(message_id)
        );
        let resp = self
            .send_authed("delete", |client, token| {
                client.delete(&url).bearer_auth(token)
            })
            .await?;
        // The delete scope is newer than the rest — a token from before it was
        // requested fails here while everything else still works, so point at
        // the fix instead of dumping the raw rejection.
        if matches!(resp.status().as_u16(), 401 | 403) {
            anyhow::bail!(
                "kick delete failed ({}): the login may predate the delete permission — \
                 log out and back in (Settings → Account)",
                resp.status()
            );
        }
        ensure_ok(resp, "delete").await
    }

    /// Unbans (or removes a timeout for) a user in a channel.
    pub async fn unban(&self, broadcaster_id: u64, target_id: u64) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "broadcaster_user_id": broadcaster_id,
            "user_id": target_id,
        });
        let resp = self
            .send_authed("unban", |client, token| {
                client.delete(BANS_URL).bearer_auth(token).json(&body)
            })
            .await?;
        ensure_ok(resp, "unban").await
    }

}

/// Turns a non-2xx response into an error carrying the body (Kick puts the
/// reason there), so failures aren't just a bare status code.
async fn ensure_ok(resp: reqwest::Response, action: &str) -> anyhow::Result<()> {
    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow!("kick {action} failed ({status}): {body}"))
}

