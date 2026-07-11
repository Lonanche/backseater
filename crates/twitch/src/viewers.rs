//! Live concurrent viewer count, fetched without authentication.
//!
//! The **one-time seed** for the status bar: the live count is pushed by Hermes
//! (`video-playback-by-id`, see [`crate::pubsub`]), but Twitch only sends a
//! frame every ~30s — so the bar would sit at a bare "LIVE" until the first
//! push. When the live poll first sees a stream live, it fetches this once. Not
//! polled beyond that: GQL's `viewersCount` only moves in coarse (~1min+)
//! buckets, so re-fetching it would flap stale values against the fresh pushes.
//! (IVR's `viewersCount` is worse still — cached for minutes.) Uses the same
//! public web client-id as [`crate::badges`] / [`crate::videos`].

use anyhow::Context;
use serde::Deserialize;

use crate::http::{GQL_URL, WEB_CLIENT_ID};

const VIEWERS_QUERY: &str =
    "query($login: String!) { user(login: $login) { stream { viewersCount } } }";

#[derive(Deserialize)]
struct GqlResponse {
    #[serde(default)]
    data: Option<Data>,
}
#[derive(Deserialize)]
struct Data {
    #[serde(default)]
    user: Option<User>,
}
#[derive(Deserialize)]
struct User {
    #[serde(default)]
    stream: Option<Stream>,
}
#[derive(Deserialize)]
struct Stream {
    #[serde(default, rename = "viewersCount")]
    viewers_count: Option<u64>,
}

/// Fetches the channel's current concurrent viewer count. `Ok(None)` when
/// offline (`stream` is null) or the count is missing.
pub async fn fetch_viewer_count(channel: &str) -> anyhow::Result<Option<u64>> {
    let login = bks_core::channel_login(channel);
    let body = serde_json::json!({
        "query": VIEWERS_QUERY,
        "variables": { "login": login },
    });
    let resp: GqlResponse = crate::http::client()
        .post(GQL_URL)
        .header("Client-Id", WEB_CLIENT_ID)
        .json(&body)
        .send()
        .await
        .context("requesting viewer count")?
        .error_for_status()
        .context("viewer-count request failed")?
        .json()
        .await
        .context("parsing viewer-count response")?;

    let count = viewer_count_from(resp);
    tracing::debug!("twitch viewer count for {login}: {count:?}");
    Ok(count)
}

/// Pure mapping from the GQL shape, so it's unit-testable without the network.
fn viewer_count_from(resp: GqlResponse) -> Option<u64> {
    resp.data?.user?.stream?.viewers_count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Option<u64> {
        viewer_count_from(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn live_stream_yields_count() {
        // Real shape (verified live).
        let json = r#"{"data":{"user":{"stream":{"viewersCount":8564}}}}"#;
        assert_eq!(parse(json), Some(8564));
    }

    #[test]
    fn offline_or_missing_yields_none() {
        assert_eq!(parse(r#"{"data":{"user":{"stream":null}}}"#), None);
        assert_eq!(parse(r#"{"data":{"user":null}}"#), None);
        assert_eq!(parse(r#"{}"#), None);
    }
}
