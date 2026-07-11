//! One shared `reqwest::Client` for all Twitch-side REST calls (Helix, IVR,
//! badges, history), so keep-alive/connection pooling is reused instead of each
//! call paying a fresh pool + TLS handshake (the live-status poll alone fires
//! every 30s per tab). Also the one place request timeouts are set — reqwest's
//! default has none, so a stalled connection would otherwise hang a moderation
//! action or usercard fetch forever.

use std::sync::OnceLock;
use std::time::Duration;

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
