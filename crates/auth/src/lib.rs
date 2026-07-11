//! OAuth login for authenticated chat + moderation, per platform.
//!
//! - [`twitch`]: implicit flow, no client secret — a built-in Client ID works
//!   out of the box.
//! - [`kick`]: authorization-code flow with PKCE; the token exchange needs a
//!   client secret, which a small Cloudflare Worker broker holds (see `worker/`)
//!   so it never ships in the binary.
//!
//! Both run a throwaway local HTTP server ([`server`]) as the redirect target
//! and persist credentials via [`store`] (the OS keyring on Windows).

pub mod kick;
mod server;
/// Persistence: JSON files in the OS config dir (`<config>/backseater/`) for app
/// data like the tab list; the OS keyring for credentials (`*_secret` fns).
pub mod store;
pub mod twitch;

/// The `reqwest::Client` for all auth-side requests (token validation, broker
/// exchange/refresh, user lookup), with timeouts — reqwest's default has none,
/// so a stalled connection would hang a login/refresh forever.
pub(crate) fn http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .expect("building auth http client")
        })
        .clone()
}
