//! Kick login: the **authorization-code** OAuth flow with **PKCE**. Kick's token
//! exchange requires a client *secret*, which must not ship in the binary — so a
//! small Cloudflare Worker broker (see `worker/`) holds the secret and performs
//! the exchange/refresh on our behalf. PKCE (`code_verifier`) authenticates the
//! request without the secret reaching users.
//!
//! Flow: fetch the public `client_id` from the broker → open
//! `/oauth/authorize?response_type=code…` → browser redirects to
//! `localhost:38275/?code=…` → POST the code (+ verifier) to the broker's
//! `/kick/token` → it returns access + refresh tokens → resolve the user id from
//! the Kick API. Tokens expire, so [`Credentials`] carries a refresh token + the
//! broker URL needed to refresh.

use anyhow::{anyhow, Context};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::server;

/// Env var to override the built-in broker base URL (no trailing slash).
pub const BROKER_URL_ENV: &str = "BKS_KICK_BROKER_URL";

/// Default broker. The broker URL is not a secret (it only fronts the Kick app's
/// public client_id and the secret-side token exchange), so it ships in the
/// binary; override with `BKS_KICK_BROKER_URL` to point at your own deployment.
const DEFAULT_BROKER_URL: &str = "https://chat-kick-broker.lonanche.workers.dev";

const REDIRECT_PORT: u16 = 38275;
const REDIRECT_URI: &str = "http://localhost:38275";
const AUTHORIZE_URL: &str = "https://id.kick.com/oauth/authorize";
const USERS_URL: &str = "https://api.kick.com/public/v1/users";
const STORE_NAME: &str = "kick_credentials";

// `moderation:chat_message:manage` = delete-message; a token from before it
// was added keeps chatting, delete just 401/403s with a re-login hint.
const SCOPES: &str = "user:read channel:read chat:write moderation:ban moderation:chat_message:manage";

/// A logged-in Kick session. Tokens expire, so we keep the refresh token + the
/// broker URL needed to refresh them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub user_id: String,
    pub username: String,
    /// The broker that minted these tokens; used to refresh them later.
    pub broker_url: String,
}

/// The broker base URL: the `BKS_KICK_BROKER_URL` override if set, else the
/// built-in default.
pub fn broker_url() -> String {
    let url = std::env::var(BROKER_URL_ENV).unwrap_or_else(|_| DEFAULT_BROKER_URL.to_string());
    url.trim_end_matches('/').to_string()
}

#[derive(Deserialize)]
struct ConfigResponse {
    client_id: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct UsersResponse {
    data: Vec<KickUser>,
}

#[derive(Deserialize)]
struct KickUser {
    user_id: u64,
    name: String,
}

/// Runs the full login: fetch client_id, PKCE + browser approval, code exchange
/// via the broker, user lookup. `broker` is the deployed broker base URL.
pub async fn login(broker: &str) -> anyhow::Result<Credentials> {
    let client_id = fetch_client_id(broker).await?;
    let verifier = server::random_token(64);
    let challenge = code_challenge(&verifier);
    let state = server::random_token(24);

    let auth_url = format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={client_id}\
         &redirect_uri={REDIRECT_URI}&scope={}&code_challenge={challenge}\
         &code_challenge_method=S256&state={state}",
        server::urlencode(SCOPES)
    );

    let listener = server::bind(REDIRECT_PORT).await?;
    open::that(&auth_url).context("opening browser for Kick login")?;
    tracing::info!("opened browser for Kick login; waiting for approval");

    // Authorization-code flow: the code is in the query, no fragment forwarding.
    let params = server::wait_for_redirect(&listener, false).await?;
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };

    if get("state").as_deref() != Some(&state) {
        return Err(anyhow!("Kick login state mismatch (possible CSRF)"));
    }
    let code = get("code").context("no code in Kick redirect")?;

    let tokens = exchange_code(broker, &code, &verifier).await?;
    let (user_id, username) = fetch_user(&tokens.access_token).await?;

    Ok(Credentials {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        user_id,
        username,
        broker_url: broker.to_string(),
    })
}

/// Refreshes an expired access token via the broker. Returns updated credentials
/// (the refresh token rotates too).
pub async fn refresh(creds: &Credentials) -> anyhow::Result<Credentials> {
    let body = serde_json::json!({ "refresh_token": creds.refresh_token });
    let tokens: TokenResponse = crate::http_client()
        .post(format!("{}/kick/refresh", creds.broker_url))
        .json(&body)
        .send()
        .await
        .context("refreshing Kick token")?
        .error_for_status()
        .context("broker refused the refresh token")?
        .json()
        .await?;

    Ok(Credentials {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        ..creds.clone()
    })
}

async fn fetch_client_id(broker: &str) -> anyhow::Result<String> {
    let resp: ConfigResponse = crate::http_client()
        .get(format!("{broker}/kick/config"))
        .send()
        .await
        .context("fetching Kick client id from broker")?
        .error_for_status()
        .context("broker config request failed")?
        .json()
        .await?;
    Ok(resp.client_id)
}

async fn exchange_code(broker: &str, code: &str, verifier: &str) -> anyhow::Result<TokenResponse> {
    let body = serde_json::json!({ "code": code, "code_verifier": verifier });
    crate::http_client()
        .post(format!("{broker}/kick/token"))
        .json(&body)
        .send()
        .await
        .context("exchanging Kick auth code via broker")?
        .error_for_status()
        .context("broker rejected the auth code")?
        .json()
        .await
        .context("parsing broker token response")
}

/// With no ids, `/users` returns the authenticated user.
async fn fetch_user(access_token: &str) -> anyhow::Result<(String, String)> {
    let resp: UsersResponse = crate::http_client()
        .get(USERS_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .context("looking up Kick user")?
        .error_for_status()?
        .json()
        .await?;
    let user = resp
        .data
        .into_iter()
        .next()
        .context("Kick returned no user")?;
    Ok((user.user_id.to_string(), user.name))
}

/// The PKCE S256 challenge: base64url(SHA-256(verifier)), no padding.
fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
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
    fn pkce_challenge_matches_rfc_test_vector() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
