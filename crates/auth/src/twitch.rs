//! Twitch login: the **implicit** OAuth flow (no client secret). Open the
//! browser, the user approves, Twitch redirects to
//! `http://localhost:38276/#access_token=…`, the [`server`](crate::server) page
//! forwards the fragment to us, and we validate the token.

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::server;

/// Env var to override the built-in Twitch Client ID (e.g. your own app).
pub const CLIENT_ID_ENV: &str = "BKS_TWITCH_CLIENT_ID";

/// Built-in Twitch Client ID. A Client ID is *not* a secret (it appears in every
/// OAuth redirect; only a Client Secret must stay private, and the implicit flow
/// uses none), so it's safe to ship in the binary. Redirect:
/// `http://localhost:38276`.
const DEFAULT_CLIENT_ID: &str = "y3jef0lbj1auyf3fraa6oa4hgiddk0";

const REDIRECT_PORT: u16 = 38276;
const AUTHORIZE_URL: &str = "https://id.twitch.tv/oauth2/authorize";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const STORE_NAME: &str = "twitch_credentials";

/// Chat + moderation, plus the EventSub moderator feed: `channel.moderate` v2
/// requires the whole read set below (each satisfied by read OR manage), the
/// suspicious-user (Low Trust) marks need `moderator:read:suspicious_users`
/// (+ `manage` for `/monitor` `/restrict` `/unmonitor` `/unrestrict`), and
/// AutoMod hold/allow/deny needs `moderator:manage:automod`. The viewer list
/// needs `moderator:read:chatters`; the personal emote set for the picker +
/// autocomplete (cross-channel sub emotes) needs `user:read:emotes`. The
/// slash commands need their manage scopes: announcements (`/announce`),
/// warnings (`/warn`), chat_settings (`/slow`, `/followers`, …), shoutouts
/// (`/shoutout`), raids (`/raid`), and the broadcaster-only
/// `channel:manage:moderators`/`vips` (`/mod`, `/vip` + the usercard buttons).
/// A token from before these were added keeps working for chat — the extra
/// features just stay off (401/403 with a hint) until the next login.
const SCOPES: &str = "chat:read chat:edit user:write:chat user:read:emotes \
                      moderator:manage:banned_users moderator:manage:chat_messages \
                      moderator:manage:automod moderator:manage:announcements \
                      moderator:manage:warnings moderator:manage:chat_settings \
                      moderator:manage:shoutouts \
                      channel:manage:raids channel:manage:moderators channel:manage:vips \
                      moderator:read:blocked_terms \
                      moderator:read:unban_requests \
                      moderator:read:moderators moderator:read:vips \
                      moderator:read:chatters moderator:read:suspicious_users \
                      moderator:manage:suspicious_users";

/// A validated Twitch user session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    /// OAuth access token (no `oauth:` prefix).
    pub access_token: String,
    pub login: String,
    pub user_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl Credentials {
    /// The IRC PASS value Twitch expects: `oauth:<token>`.
    pub fn irc_pass(&self) -> String {
        format!("oauth:{}", self.access_token)
    }
}

#[derive(Deserialize)]
struct ValidateResponse {
    login: String,
    user_id: String,
    #[serde(default)]
    scopes: Vec<String>,
}

/// The Client ID to use: `BKS_TWITCH_CLIENT_ID` if set, else the built-in default.
pub fn client_id() -> String {
    std::env::var(CLIENT_ID_ENV).unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string())
}

/// Runs the full login: opens the browser, waits for the redirect, validates
/// the token. `client_id` must have `http://localhost:38276` as a redirect URL.
pub async fn login(client_id: &str) -> anyhow::Result<Credentials> {
    let redirect = format!("http://localhost:{REDIRECT_PORT}");
    // `state` ties the redirect to this login attempt: without it, anything that
    // can reach localhost:38276 during a login (another local process, a web page
    // firing a request at localhost) could inject a *different* account's valid
    // token — `validate` only proves the token is real, not that it's ours.
    // Twitch echoes it back in the fragment, which the bootstrap page forwards.
    let state = server::random_token(24);
    let auth_url = format!(
        "{AUTHORIZE_URL}?response_type=token&client_id={client_id}\
         &redirect_uri={redirect}&scope={}&state={state}",
        server::urlencode(SCOPES)
    );

    let listener = server::bind(REDIRECT_PORT).await?;
    open::that(&auth_url).context("opening browser for Twitch login")?;
    tracing::info!("opened browser for Twitch login; waiting for approval");

    // Implicit flow: the token is in the URL fragment, so forward it via JS.
    let params = server::wait_for_redirect(&listener, true).await?;
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    if get("state").as_deref() != Some(&state) {
        return Err(anyhow::anyhow!(
            "Twitch login state mismatch (possible CSRF)"
        ));
    }
    let token = get("access_token").context("no access_token in Twitch redirect")?;

    validate(client_id, &token).await
}

/// Validates the token against Twitch and resolves login + user id + scopes.
async fn validate(client_id: &str, token: &str) -> anyhow::Result<Credentials> {
    let resp = crate::http_client()
        .get(VALIDATE_URL)
        .header("Authorization", format!("OAuth {token}"))
        .header("Client-Id", client_id)
        .send()
        .await
        .context("validating token")?
        .error_for_status()
        .context("token rejected by Twitch")?
        .json::<ValidateResponse>()
        .await
        .context("parsing validate response")?;

    Ok(Credentials {
        access_token: token.to_string(),
        login: resp.login,
        user_id: resp.user_id,
        scopes: resp.scopes,
    })
}

pub fn save(creds: &Credentials) -> anyhow::Result<()> {
    crate::store::save_secret(STORE_NAME, creds)
}

pub fn load() -> anyhow::Result<Option<Credentials>> {
    crate::store::load_secret(STORE_NAME)
}

pub fn clear() -> anyhow::Result<()> {
    crate::store::clear_secret(STORE_NAME)
}
