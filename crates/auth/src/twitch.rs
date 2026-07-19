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

/// The always-requested core: read + send chat (IRC), Helix sends (`/pin`'s
/// send-and-pin), and the personal emote set for the picker + autocomplete
/// (cross-channel sub emotes).
const CHAT_SCOPES: &[&str] = &[
    "chat:read",
    "chat:edit",
    "user:write:chat",
    "user:read:emotes",
];

/// Basic moderation on top of chat: ban/timeout/unban (+ the usercard's timeout
/// chips), single-message delete + `/clear`, pin/unpin, and the viewer list.
const BASIC_MOD_SCOPES: &[&str] = &[
    "moderator:manage:banned_users",
    "moderator:manage:chat_messages",
    "moderator:read:chatters",
];

/// Everything else a moderator can do, on top of basic: warnings, announcements,
/// shoutouts, chat modes (`/slow`, `/followers`, …), the AutoMod queue, the
/// suspicious-user (Low Trust) marks + `/monitor`/`/restrict`, and the read set
/// the EventSub `channel.moderate` v2 rich moderator feed requires (each of its
/// conditions is satisfied by read OR manage, so the manage scopes above cover
/// banned_users/chat_messages and the reads below cover the rest).
const FULL_MOD_SCOPES: &[&str] = &[
    "moderator:manage:automod",
    "moderator:manage:announcements",
    "moderator:manage:warnings",
    "moderator:manage:chat_settings",
    "moderator:manage:shoutouts",
    "moderator:read:blocked_terms",
    "moderator:read:unban_requests",
    "moderator:read:moderators",
    "moderator:read:vips",
    "moderator:read:suspicious_users",
    "moderator:manage:suspicious_users",
];

/// Own-channel commands, a separate opt-in (only useful to streamers): `/raid`
/// `/unraid`, `/mod` `/unmod`, `/vip` `/unvip` + the usercard role buttons.
const BROADCASTER_SCOPES: &[&str] = &[
    "channel:manage:raids",
    "channel:manage:moderators",
    "channel:manage:vips",
];

/// How much of Twitch the user chose to authorize at login — the consent screen
/// grows with the tier, which is why this is a choice at all (the full list is
/// long enough to scare off someone who only wants to chat). Each tier includes
/// the ones below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopePreset {
    ChatOnly,
    BasicModeration,
    #[default]
    FullModerator,
}

/// The user's scope selection for a Twitch login: a preset tier plus the
/// separate broadcaster add-on (raids + role grants — only useful on a channel
/// they own).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeChoice {
    pub preset: ScopePreset,
    #[serde(default)]
    pub broadcaster: bool,
}

impl Default for ScopeChoice {
    /// Everything — what logins requested before the choice existed.
    fn default() -> Self {
        Self {
            preset: ScopePreset::FullModerator,
            broadcaster: true,
        }
    }
}

impl ScopeChoice {
    /// The scopes this choice requests, in a stable order.
    pub fn scopes(&self) -> Vec<&'static str> {
        let mut scopes: Vec<&'static str> = CHAT_SCOPES.to_vec();
        if self.preset != ScopePreset::ChatOnly {
            scopes.extend_from_slice(BASIC_MOD_SCOPES);
        }
        if self.preset == ScopePreset::FullModerator {
            scopes.extend_from_slice(FULL_MOD_SCOPES);
        }
        if self.broadcaster {
            scopes.extend_from_slice(BROADCASTER_SCOPES);
        }
        scopes
    }

    /// The space-separated scope string for the authorize URL.
    fn scope_string(&self) -> String {
        self.scopes().join(" ")
    }

    /// A short human summary of what a *granted* scope list amounts to, for the
    /// account row ("chat only", "basic moderation", …). Derived from the token,
    /// not the saved choice, so it's honest about what Twitch actually granted
    /// (an old token from before the tiers reads as whatever it carries).
    pub fn summarize(granted: &[String]) -> String {
        let has = |s: &str| granted.iter().any(|x| x == s);
        let tier = if FULL_MOD_SCOPES.iter().all(|s| has(s)) {
            "full moderator"
        } else if BASIC_MOD_SCOPES.iter().all(|s| has(s)) {
            "basic moderation"
        } else {
            "chat only"
        };
        if BROADCASTER_SCOPES.iter().all(|s| has(s)) {
            format!("{tier} + broadcaster")
        } else {
            tier.to_string()
        }
    }
}

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
/// `choice` picks how much is requested (the consent screen lists exactly it).
pub async fn login(client_id: &str, choice: ScopeChoice) -> anyhow::Result<Credentials> {
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
        server::urlencode(&choice.scope_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(scopes: &[&str]) -> Vec<String> {
        scopes.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn tiers_nest_and_default_is_everything() {
        let chat = ScopeChoice {
            preset: ScopePreset::ChatOnly,
            broadcaster: false,
        }
        .scopes();
        let basic = ScopeChoice {
            preset: ScopePreset::BasicModeration,
            broadcaster: false,
        }
        .scopes();
        let full = ScopeChoice {
            preset: ScopePreset::FullModerator,
            broadcaster: false,
        }
        .scopes();
        assert_eq!(chat, CHAT_SCOPES);
        assert!(chat.iter().all(|s| basic.contains(s)));
        assert!(basic.iter().all(|s| full.contains(s)));
        assert!(basic.contains(&"moderator:read:chatters"));
        assert!(!basic.contains(&"moderator:manage:warnings"));
        assert!(!full.iter().any(|s| s.starts_with("channel:manage:")));

        // The default choice (pre-existing behavior) requests every scope, and
        // nothing twice.
        let all = ScopeChoice::default().scopes();
        assert!(all.contains(&"channel:manage:raids"));
        let mut deduped = all.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(all.len(), deduped.len());
    }

    #[test]
    fn full_moderator_satisfies_the_eventsub_moderate_feed() {
        // channel.moderate v2 wants read-or-manage of each of these; the full
        // tier must cover all of them or the rich moderator feed silently
        // stays off (see bks_twitch::eventsub::can_moderate_feed).
        let full = ScopeChoice {
            preset: ScopePreset::FullModerator,
            broadcaster: false,
        }
        .scopes();
        let covered = |name: &str| {
            full.contains(&format!("moderator:read:{name}").as_str())
                || full.contains(&format!("moderator:manage:{name}").as_str())
        };
        for name in [
            "blocked_terms",
            "chat_settings",
            "unban_requests",
            "banned_users",
            "chat_messages",
            "warnings",
        ] {
            assert!(covered(name), "feed scope not covered: {name}");
        }
        assert!(full.contains(&"moderator:read:moderators"));
        assert!(full.contains(&"moderator:read:vips"));
    }

    #[test]
    fn summarize_names_the_granted_tier() {
        let chat = ScopeChoice {
            preset: ScopePreset::ChatOnly,
            broadcaster: false,
        };
        let basic = ScopeChoice {
            preset: ScopePreset::BasicModeration,
            broadcaster: false,
        };
        assert_eq!(ScopeChoice::summarize(&owned(&chat.scopes())), "chat only");
        assert_eq!(
            ScopeChoice::summarize(&owned(&basic.scopes())),
            "basic moderation"
        );
        assert_eq!(
            ScopeChoice::summarize(&owned(&ScopeChoice::default().scopes())),
            "full moderator + broadcaster"
        );
        // A legacy token (no scopes stored) reads as the floor.
        assert_eq!(ScopeChoice::summarize(&[]), "chat only");
    }
}
