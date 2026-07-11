//! One shared `reqwest::Client` for all Twitch-side REST calls (Helix, IVR,
//! badges, history), so keep-alive/connection pooling is reused instead of each
//! call paying a fresh pool + TLS handshake (the live-status poll alone fires
//! every 30s per tab). Also the one place request timeouts are set — reqwest's
//! default has none, so a stalled connection would otherwise hang a moderation
//! action or usercard fetch forever.

use std::sync::OnceLock;
use std::time::Duration;

/// Public web client-id Twitch's own site uses for unauthenticated GraphQL —
/// shared by every anonymous GQL call (badges, last-broadcast, viewer count).
pub(crate) const WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
pub(crate) const GQL_URL: &str = "https://gql.twitch.tv/gql";

pub(crate) fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("building twitch http client")
        })
        .clone()
}
