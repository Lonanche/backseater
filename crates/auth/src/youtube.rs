//! YouTube login: Google's **authorization-code + PKCE** flow for desktop
//! apps, with a loopback redirect (`http://127.0.0.1:<port>`).
//!
//! Google classes desktop OAuth clients as *public*: their "client secret"
//! ships inside every installed app and isn't treated as confidential, so —
//! unlike Kick — no broker is needed; PKCE protects the code exchange. The
//! token endpoint still expects the secret, so it rides along with the id.
//!
//! The client is the built-in one compiled into release builds; the
//! `BKS_YOUTUBE_CLIENT_ID` / `BKS_YOUTUBE_CLIENT_SECRET` env vars override it
//! at runtime (how a local dev build, which has none built in, logs in).

use anyhow::{anyhow, Context};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::server;

pub const CLIENT_ID_ENV: &str = "BKS_YOUTUBE_CLIENT_ID";
pub const CLIENT_SECRET_ENV: &str = "BKS_YOUTUBE_CLIENT_SECRET";

/// The built-in desktop OAuth client, baked in at **compile time** from the
/// `BKS_BUILTIN_YOUTUBE_CLIENT_ID` / `_SECRET` build env vars (CI sets them
/// from repository secrets for release builds). Not confidential — Google
/// treats a desktop client's secret as public — but kept out of the repo so
/// forks and scrapers don't spend the project's quota. A build without them
/// (local `cargo run`, fork PRs) has no built-in client and needs the runtime
/// env vars to log in.
const DEFAULT_CLIENT_ID: &str = match option_env!("BKS_BUILTIN_YOUTUBE_CLIENT_ID") {
    Some(id) => id,
    None => "",
};
const DEFAULT_CLIENT_SECRET: &str = match option_env!("BKS_BUILTIN_YOUTUBE_CLIENT_SECRET") {
    Some(secret) => secret,
    None => "",
};

const REDIRECT_PORT: u16 = 38277;
const REDIRECT_URI: &str = "http://127.0.0.1:38277";
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const CHANNELS_URL: &str = "https://www.googleapis.com/youtube/v3/channels";
/// Send messages + delete messages + ban/unban (`liveChatMessages`/`liveChatBans`).
const SCOPE: &str = "https://www.googleapis.com/auth/youtube.force-ssl";
const STORE_NAME: &str = "youtube_credentials";

/// A Google OAuth desktop client (id + its non-confidential secret).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleClient {
    pub id: String,
    pub secret: String,
}

impl GoogleClient {
    fn is_complete(&self) -> bool {
        !self.id.trim().is_empty() && !self.secret.trim().is_empty()
    }
}

/// A logged-in YouTube session. Access tokens last ~1h, so the refresh token +
/// the client that minted it are kept to renew them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds when `access_token` expires.
    pub expires_at: i64,
    /// The account's own channel (`UC…`) — what its chat messages carry.
    pub channel_id: String,
    pub channel_title: String,
    pub client: GoogleClient,
}

/// The client to log in with: the env override if set, else the built-in one
/// — `None` in a build that has neither.
pub fn active_client() -> Option<GoogleClient> {
    let env = GoogleClient {
        id: std::env::var(CLIENT_ID_ENV).unwrap_or_default(),
        secret: std::env::var(CLIENT_SECRET_ENV).unwrap_or_default(),
    };
    if env.is_complete() {
        return Some(env);
    }
    let built_in = GoogleClient {
        id: DEFAULT_CLIENT_ID.into(),
        secret: DEFAULT_CLIENT_SECRET.into(),
    };
    built_in.is_complete().then_some(built_in)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Runs the full login: PKCE + browser consent, code exchange, own-channel
/// lookup. Fails when the Google account has no YouTube channel (it couldn't
/// chat anyway).
pub async fn login(client: GoogleClient) -> anyhow::Result<Credentials> {
    let verifier = server::random_token(64);
    let challenge = code_challenge(&verifier);
    let state = server::random_token(24);

    // `access_type=offline` + `prompt=consent` → Google always returns a
    // refresh token, even on a re-login of an already-consented account.
    let auth_url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}\
         &scope={}&code_challenge={challenge}&code_challenge_method=S256\
         &state={state}&access_type=offline&prompt=consent",
        server::urlencode(&client.id),
        server::urlencode(REDIRECT_URI),
        server::urlencode(SCOPE),
    );

    let listener = server::bind(REDIRECT_PORT).await?;
    open::that(&auth_url).context("opening browser for YouTube login")?;
    tracing::info!("opened browser for YouTube login; waiting for approval");

    let params = server::wait_for_redirect(&listener, false).await?;
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| percent_decode(v))
    };
    if let Some(err) = get("error") {
        return Err(anyhow!("Google refused the login: {err}"));
    }
    if get("state").as_deref() != Some(&state) {
        return Err(anyhow!("YouTube login state mismatch (possible CSRF)"));
    }
    let code = get("code").context("no code in Google redirect")?;

    let tokens: TokenResponse = token_request(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("code_verifier", &verifier),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", &client.id),
        ("client_secret", &client.secret),
    ])
    .await
    .context("exchanging Google auth code")?;
    let refresh_token = tokens
        .refresh_token
        .context("Google returned no refresh token")?;
    let (channel_id, channel_title) = fetch_own_channel(&tokens.access_token).await?;

    Ok(Credentials {
        access_token: tokens.access_token,
        refresh_token,
        expires_at: expiry(tokens.expires_in),
        channel_id,
        channel_title,
        client,
    })
}

/// Renews the access token. Google keeps the refresh token unless it rotates
/// one in the response. An `invalid_grant` (revoked / expired grant) is an
/// error the caller should treat as logged out.
pub async fn refresh(creds: &Credentials) -> anyhow::Result<Credentials> {
    let tokens: TokenResponse = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &creds.refresh_token),
        ("client_id", &creds.client.id),
        ("client_secret", &creds.client.secret),
    ])
    .await
    .context("refreshing YouTube token")?;
    Ok(Credentials {
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .unwrap_or_else(|| creds.refresh_token.clone()),
        expires_at: expiry(tokens.expires_in),
        ..creds.clone()
    })
}

/// Best-effort revoke on logout, so the grant doesn't linger in the user's
/// Google account permissions.
pub async fn revoke(creds: &Credentials) {
    let result = crate::http_client()
        .post(REVOKE_URL)
        .form(&[("token", creds.refresh_token.as_str())])
        .send()
        .await;
    if let Err(err) = result {
        tracing::debug!("revoking YouTube token failed: {err:#}");
    }
}

async fn token_request(form: &[(&str, &str)]) -> anyhow::Result<TokenResponse> {
    let resp = crate::http_client()
        .post(TOKEN_URL)
        .form(form)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("{status}: {}", google_error_text(&body)));
    }
    Ok(resp.json().await?)
}

/// The `error` / `error_description` of a Google OAuth error body, else the raw body.
fn google_error_text(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    match (value["error"].as_str(), value["error_description"].as_str()) {
        (Some(err), Some(desc)) => format!("{err} ({desc})"),
        (Some(err), None) => err.to_string(),
        _ => body.to_string(),
    }
}

async fn fetch_own_channel(access_token: &str) -> anyhow::Result<(String, String)> {
    let resp: serde_json::Value = crate::http_client()
        .get(CHANNELS_URL)
        .query(&[("part", "snippet"), ("mine", "true")])
        .bearer_auth(access_token)
        .send()
        .await
        .context("looking up the YouTube channel")?
        .error_for_status()
        .context("YouTube channel lookup failed")?
        .json()
        .await?;
    let item = resp["items"]
        .as_array()
        .and_then(|items| items.first())
        .context("this Google account has no YouTube channel — create one to chat")?;
    let id = item["id"].as_str().unwrap_or_default().to_string();
    let title = item["snippet"]["title"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    anyhow::ensure!(!id.is_empty(), "YouTube returned a channel without an id");
    Ok((id, title))
}

fn expiry(expires_in: Option<i64>) -> i64 {
    now_secs() + expires_in.unwrap_or(3600)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The PKCE S256 challenge: base64url(SHA-256(verifier)), no padding.
fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Decodes `%XX` escapes and `+` in a redirect query value (Google's codes
/// contain `/`, sent as `%2F`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_google_codes() {
        assert_eq!(percent_decode("4%2F0AbC-d_e"), "4/0AbC-d_e");
        assert_eq!(percent_decode("a+b%20c"), "a b c");
        assert_eq!(percent_decode("trailing%2"), "trailing%2");
    }

    #[test]
    fn google_error_text_prefers_description() {
        assert_eq!(
            google_error_text(r#"{"error":"invalid_grant","error_description":"Bad Request"}"#),
            "invalid_grant (Bad Request)"
        );
        assert_eq!(google_error_text("plain"), "plain");
    }
}
