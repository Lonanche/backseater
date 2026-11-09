//! Authenticated YouTube chat actions over the official **Data API v3**: send,
//! delete, ban/timeout, unban. Reading stays on anonymous InnerTube (no quota);
//! only these writes spend the Google project's quota (10,000 units/day by
//! default — send/delete 20, ban/unban 200, chat-id lookup 1).
//!
//! The Data API addresses a chat by its `liveChatId`, which comes from
//! `videos.list` on the live video the reader is following
//! ([`crate::seen::live_video`], else resolved fresh). Message ids differ from
//! InnerTube's, so a delete re-finds the message via `liveChatMessages.list` by
//! author + timestamp. Unbans need the ban resource id that only the ban
//! response carries, so only bans made this session can be lifted here.
//!
//! The access token lasts ~1h: it's refreshed shortly before expiry and once
//! more on a `401`; a refresh refused with `invalid_grant` is [`AuthExpired`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const API: &str = "https://www.googleapis.com/youtube/v3";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// YouTube rejects chat messages longer than this.
pub const MAX_MESSAGE_CHARS: usize = 200;
/// How long a resolved `liveChatId` is trusted before re-checking (a chat-gone
/// error drops it sooner).
const CHAT_ID_TTL: Duration = Duration::from_secs(600);
/// How far a Data API message's `publishedAt` may sit from InnerTube's
/// timestamp and still count as the same message.
const MATCH_WINDOW_SECS: i64 = 10;

/// Called after a token refresh with `(access_token, refresh_token, expires_at)`
/// so the caller can persist the renewed login.
pub type OnRefreshed = Arc<dyn Fn(String, String, i64) + Send + Sync>;

/// The refresh token was refused (revoked/expired grant) — log out of YouTube.
#[derive(Debug)]
pub struct AuthExpired;

impl std::fmt::Display for AuthExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("YouTube login expired — please log in again (Settings → Account)")
    }
}

impl std::error::Error for AuthExpired {}

struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
}

/// Authenticated YouTube client for one logged-in account.
pub struct YoutubeActions {
    client: reqwest::Client,
    tokens: Mutex<Tokens>,
    client_id: String,
    client_secret: String,
    on_refreshed: std::sync::Mutex<Option<OnRefreshed>>,
    /// source → (video id, liveChatId, when resolved).
    chat_ids: std::sync::Mutex<HashMap<String, (String, String, Instant)>>,
    /// (liveChatId, banned channel id) → ban resource id, for unban.
    bans: std::sync::Mutex<HashMap<(String, String), String>>,
}

impl YoutubeActions {
    pub fn new(
        access_token: String,
        refresh_token: String,
        expires_at: i64,
        client_id: String,
        client_secret: String,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("building youtube actions client"),
            tokens: Mutex::new(Tokens {
                access_token,
                refresh_token,
                expires_at,
            }),
            client_id,
            client_secret,
            on_refreshed: std::sync::Mutex::new(None),
            chat_ids: std::sync::Mutex::new(HashMap::new()),
            bans: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn set_on_refreshed(&self, cb: OnRefreshed) {
        *self.on_refreshed.lock().unwrap() = Some(cb);
    }

    /// Posts `text` to `source`'s live chat.
    pub async fn send(&self, source: &str, text: &str) -> anyhow::Result<()> {
        let chars = text.chars().count();
        anyhow::ensure!(
            chars <= MAX_MESSAGE_CHARS,
            "YouTube messages are limited to {MAX_MESSAGE_CHARS} characters (this one has {chars})"
        );
        self.with_chat(source, |chat_id| {
            let body = json!({
                "snippet": {
                    "liveChatId": chat_id,
                    "type": "textMessageEvent",
                    "textMessageDetails": { "messageText": text },
                }
            });
            async move {
                self.call(
                    reqwest::Method::POST,
                    "liveChat/messages",
                    &[("part", "snippet")],
                    Some(body),
                )
                .await
                .map(drop)
            }
        })
        .await
    }

    /// Deletes the message the reader saw as `message_id` (an InnerTube id).
    pub async fn delete_message(&self, source: &str, message_id: &str) -> anyhow::Result<()> {
        let seen = crate::seen::message(message_id).context(
            "that message is no longer known — only messages seen this session can be deleted",
        )?;
        self.with_chat(source, |chat_id| {
            let seen = seen.clone();
            async move {
                let list = self
                    .call(
                        reqwest::Method::GET,
                        "liveChat/messages",
                        &[
                            ("liveChatId", chat_id.as_str()),
                            ("part", "snippet,authorDetails"),
                            ("maxResults", "2000"),
                        ],
                        None,
                    )
                    .await?;
                let id = find_message(&list, message_id, &seen)
                    .context("couldn't find that message in YouTube's recent chat (too old?)")?;
                self.call(
                    reqwest::Method::DELETE,
                    "liveChat/messages",
                    &[("id", id.as_str())],
                    None,
                )
                .await
                .map(drop)
            }
        })
        .await
    }

    /// Bans `channel_id` from `source`'s chat — permanently, or for
    /// `duration_secs` (a timeout).
    pub async fn ban(
        &self,
        source: &str,
        channel_id: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<()> {
        self.with_chat(source, |chat_id| async move {
            let mut snippet = json!({
                "liveChatId": chat_id,
                "type": if duration_secs.is_some() { "temporary" } else { "permanent" },
                "bannedUserDetails": { "channelId": channel_id },
            });
            if let Some(secs) = duration_secs {
                snippet["banDurationSeconds"] = json!(secs);
            }
            let resp = self
                .call(
                    reqwest::Method::POST,
                    "liveChat/bans",
                    &[("part", "snippet")],
                    Some(json!({ "snippet": snippet })),
                )
                .await?;
            if let Some(ban_id) = resp["id"].as_str() {
                self.bans.lock().unwrap().insert(
                    (chat_id.clone(), channel_id.to_string()),
                    ban_id.to_string(),
                );
            }
            Ok(())
        })
        .await
    }

    /// Lifts a ban/timeout this session placed on `channel_id`.
    pub async fn unban(&self, source: &str, channel_id: &str) -> anyhow::Result<()> {
        self.with_chat(source, |chat_id| async move {
            let ban_id = self
                .bans
                .lock()
                .unwrap()
                .remove(&(chat_id.clone(), channel_id.to_string()))
                .context(
                    "YouTube only lets an app lift bans it made itself — this user wasn't \
                     banned from Backseater this session (unban them on youtube.com)",
                )?;
            self.call(
                reqwest::Method::DELETE,
                "liveChat/bans",
                &[("id", ban_id.as_str())],
                None,
            )
            .await
            .map(drop)
        })
        .await
    }

    /// Runs `op` with `source`'s `liveChatId`; if YouTube says that chat is
    /// gone (the stream restarted), re-resolves once and retries.
    async fn with_chat<F, Fut>(&self, source: &str, op: F) -> anyhow::Result<()>
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        let chat_id = self.live_chat_id(source).await?;
        match op(chat_id).await {
            Err(err) if is_chat_gone(&err) => {
                self.chat_ids.lock().unwrap().remove(source);
                let chat_id = self.live_chat_id(source).await?;
                op(chat_id).await
            }
            other => other,
        }
    }

    async fn live_chat_id(&self, source: &str) -> anyhow::Result<String> {
        let reader_video = crate::seen::live_video(source);
        if let Some((video, chat, at)) = self.chat_ids.lock().unwrap().get(source) {
            let same_video = reader_video.as_deref().is_none_or(|v| v == video);
            if same_video && at.elapsed() < CHAT_ID_TTL {
                return Ok(chat.clone());
            }
        }
        let video_id = match reader_video {
            Some(id) => id,
            None => crate::resolve::resolve_live_video_id(source)
                .await
                .context("this YouTube channel isn't live right now")?,
        };
        let resp = self
            .call(
                reqwest::Method::GET,
                "videos",
                &[("part", "liveStreamingDetails"), ("id", video_id.as_str())],
                None,
            )
            .await?;
        let chat_id = resp["items"][0]["liveStreamingDetails"]["activeLiveChatId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("this stream has no active live chat")?
            .to_string();
        self.chat_ids.lock().unwrap().insert(
            source.to_string(),
            (video_id, chat_id.clone(), Instant::now()),
        );
        Ok(chat_id)
    }

    /// One Data API call; refreshes the token first when it's about to expire
    /// and retries once on a `401`.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let mut token = self.fresh_token().await?;
        for attempt in 0..2 {
            let mut req = self
                .client
                .request(method.clone(), format!("{API}/{path}"))
                .query(query)
                .bearer_auth(&token);
            if let Some(body) = &body {
                req = req.json(body);
            }
            let resp = req.send().await.context("calling the YouTube API")?;
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                token = self.refresh(&token).await?;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(api_error(status, &text));
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).context("parsing YouTube API response");
        }
        unreachable!("the loop returns on its second attempt")
    }

    async fn fresh_token(&self) -> anyhow::Result<String> {
        let (token, expires_at) = {
            let t = self.tokens.lock().await;
            (t.access_token.clone(), t.expires_at)
        };
        if expires_at - 60 > now_secs() {
            return Ok(token);
        }
        self.refresh(&token).await
    }

    /// Refreshes unless another caller already replaced `stale` meanwhile.
    async fn refresh(&self, stale: &str) -> anyhow::Result<String> {
        let mut tokens = self.tokens.lock().await;
        if tokens.access_token != stale && tokens.expires_at - 60 > now_secs() {
            return Ok(tokens.access_token.clone());
        }
        let resp = self
            .client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", tokens.refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await
            .context("refreshing the YouTube token")?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            if body["error"].as_str() == Some("invalid_grant") {
                return Err(anyhow!(AuthExpired));
            }
            return Err(anyhow!(
                "refreshing the YouTube token failed: {status} {body}"
            ));
        }
        let access = body["access_token"]
            .as_str()
            .context("token refresh returned no access token")?
            .to_string();
        if let Some(rotated) = body["refresh_token"].as_str() {
            tokens.refresh_token = rotated.to_string();
        }
        tokens.access_token = access.clone();
        tokens.expires_at = now_secs() + body["expires_in"].as_i64().unwrap_or(3600);
        let cb = self.on_refreshed.lock().unwrap().clone();
        if let Some(cb) = cb {
            cb(
                access.clone(),
                tokens.refresh_token.clone(),
                tokens.expires_at,
            );
        }
        Ok(access)
    }
}

/// A Data API error with Google's message + reason, plus a plain-language hint
/// for the common ones.
#[derive(Debug)]
struct ApiError {
    status: reqwest::StatusCode,
    reason: String,
    message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hint = match self.reason.as_str() {
            "quotaExceeded" | "dailyLimitExceeded" => {
                " — the Google project's daily YouTube API quota is used up (resets at \
                 midnight Pacific); a Google client of your own (Settings → Account) gets its own"
            }
            "forbidden" | "insufficientPermissions" => {
                " — YouTube won't let this account do that here (moderation needs the \
                 channel owner or a moderator; chatting can be restricted or members-only)"
            }
            "liveChatEnded" => " — this stream's chat has ended",
            "liveChatDisabled" => " — chat is disabled on this stream",
            "rateLimitExceeded" => " — sending too fast, slow down",
            _ => "",
        };
        write!(
            f,
            "YouTube {}: {}{hint}",
            self.status.as_u16(),
            self.message
        )
    }
}

impl std::error::Error for ApiError {}

fn api_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let err = &value["error"];
    anyhow!(ApiError {
        status,
        reason: err["errors"][0]["reason"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        message: err["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| body.chars().take(200).collect()),
    })
}

fn is_chat_gone(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>().is_some_and(|e| {
        matches!(
            e.reason.as_str(),
            "liveChatEnded" | "liveChatNotFound" | "liveChatDisabled"
        )
    })
}

/// The Data API id of the message the reader knows as `innertube_id`: an exact
/// id match if the two ever coincide, else the same author's message closest
/// in time (within [`MATCH_WINDOW_SECS`]), text-equal ones first.
fn find_message(
    list: &Value,
    innertube_id: &str,
    seen: &crate::seen::SeenMessage,
) -> Option<String> {
    let items = list["items"].as_array()?;
    if let Some(item) = items
        .iter()
        .find(|i| i["id"].as_str() == Some(innertube_id))
    {
        return item["id"].as_str().map(str::to_string);
    }
    items
        .iter()
        .filter(|i| i["authorDetails"]["channelId"].as_str() == Some(&seen.author_channel_id))
        .filter_map(|i| {
            let published = i["snippet"]["publishedAt"]
                .as_str()
                .and_then(bks_core::parse_rfc3339)?;
            let gap = (published - seen.timestamp).num_seconds().abs();
            (gap <= MATCH_WINDOW_SECS).then(|| {
                let text_differs = i["snippet"]["displayMessage"].as_str().map(str::trim)
                    != Some(seen.text.trim());
                ((text_differs, gap), i["id"].as_str())
            })
        })
        .min_by_key(|(key, _)| *key)
        .and_then(|(_, id)| id.map(str::to_string))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(text: &str, at: &str) -> crate::seen::SeenMessage {
        crate::seen::SeenMessage {
            author_channel_id: "UCauthor".into(),
            timestamp: bks_core::parse_rfc3339(at).unwrap(),
            text: text.into(),
        }
    }

    fn item(id: &str, author: &str, at: &str, text: &str) -> Value {
        json!({
            "id": id,
            "snippet": { "publishedAt": at, "displayMessage": text },
            "authorDetails": { "channelId": author },
        })
    }

    #[test]
    fn finds_by_author_time_and_text() {
        let list = json!({ "items": [
            item("LCC.other", "UCother", "2026-11-08T12:00:01Z", "hi"),
            item("LCC.early", "UCauthor", "2026-11-08T12:00:00Z", "first"),
            item("LCC.same", "UCauthor", "2026-11-08T12:00:03Z", "hi"),
            item("LCC.late", "UCauthor", "2026-11-08T12:05:00Z", "hi"),
        ]});
        let target = seen("hi", "2026-11-08T12:00:02Z");
        assert_eq!(
            find_message(&list, "Chw…", &target).as_deref(),
            Some("LCC.same")
        );
    }

    #[test]
    fn exact_id_wins_and_out_of_window_misses() {
        let list = json!({ "items": [
            item("abc", "UCother", "2026-11-08T13:00:00Z", "x"),
            item("LCC.late", "UCauthor", "2026-11-08T12:05:00Z", "hi"),
        ]});
        let target = seen("hi", "2026-11-08T12:00:00Z");
        assert_eq!(find_message(&list, "abc", &target).as_deref(), Some("abc"));
        assert_eq!(find_message(&list, "zzz", &target), None);
    }

    #[test]
    fn api_errors_carry_reason_hints() {
        let err = api_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"code":403,"message":"Quota exceeded.","errors":[{"reason":"quotaExceeded"}]}}"#,
        );
        assert!(format!("{err}").contains("quota is used up"));
        assert!(!is_chat_gone(&err));
        let gone = api_error(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"error":{"message":"ended","errors":[{"reason":"liveChatEnded"}]}}"#,
        );
        assert!(is_chat_gone(&gone));
    }
}
