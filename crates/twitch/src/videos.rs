//! The channel's most recent past broadcast, fetched without authentication.
//!
//! IVR's `lastBroadcast` gives only a start time + title; the website's own
//! GraphQL endpoint (same public web client-id as [`crate::badges`]) also has
//! the broadcast's **length** and category via the channel's newest archive
//! VOD — enough for the tooltip's full "last live X ago, for Y" line. Channels
//! with VOD archiving disabled return no videos (the caller falls back to the
//! IVR data).

use anyhow::Context;
use bks_platform::LastStream;
use serde::Deserialize;

/// Public web client-id Twitch's own site uses for unauthenticated GraphQL.
const WEB_CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const GQL_URL: &str = "https://gql.twitch.tv/gql";

/// The newest archive VOD's title / start / length / category.
const LAST_ARCHIVE_QUERY: &str = "query($login: String!) { user(login: $login) { \
     videos(first: 1, type: ARCHIVE, sort: TIME) { edges { node { \
     title createdAt lengthSeconds game { displayName } } } } } }";

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
    videos: Option<Videos>,
}
#[derive(Deserialize)]
struct Videos {
    #[serde(default)]
    edges: Vec<Edge>,
}
#[derive(Deserialize)]
struct Edge {
    node: Node,
}
#[derive(Deserialize)]
struct Node {
    #[serde(default)]
    title: String,
    #[serde(default, rename = "createdAt")]
    created_at: String,
    #[serde(default, rename = "lengthSeconds")]
    length_seconds: i64,
    #[serde(default)]
    game: Option<Game>,
}
#[derive(Deserialize)]
struct Game {
    #[serde(default, rename = "displayName")]
    display_name: String,
}

/// Fetches the channel's most recent past broadcast (its newest archive VOD).
/// `Ok(None)` when the channel has none (VODs disabled / never streamed).
pub async fn fetch_last_stream(channel: &str) -> anyhow::Result<Option<LastStream>> {
    let login = bks_core::channel_login(channel);
    let body = serde_json::json!({
        "query": LAST_ARCHIVE_QUERY,
        "variables": { "login": login },
    });
    let resp: GqlResponse = crate::http::client()
        .post(GQL_URL)
        .header("Client-Id", WEB_CLIENT_ID)
        .json(&body)
        .send()
        .await
        .context("requesting last archive VOD")?
        .error_for_status()
        .context("last-archive request failed")?
        .json()
        .await
        .context("parsing last-archive response")?;

    Ok(last_stream_from_response(resp))
}

/// Pure mapping from the GQL shape, so it's unit-testable without the network.
fn last_stream_from_response(resp: GqlResponse) -> Option<LastStream> {
    let node = resp.data?.user?.videos?.edges.into_iter().next()?.node;
    let started_at = bks_core::parse_rfc3339(&node.created_at)?;
    Some(LastStream {
        started_at,
        ended_at: Some(started_at + chrono::Duration::seconds(node.length_seconds.max(0))),
        title: node.title,
        game: node.game.map(|g| g.display_name).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Option<LastStream> {
        last_stream_from_response(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn archive_vod_maps_to_last_stream() {
        let json = r#"{"data":{"user":{"videos":{"edges":[{"node":{
            "title":"Games and shit!","createdAt":"2026-07-02T13:00:40Z",
            "lengthSeconds":21729,"game":{"displayName":"Just Chatting"}}}]}}}}"#;
        let last = parse(json).expect("a last stream");
        assert_eq!(last.title, "Games and shit!");
        assert_eq!(last.game, "Just Chatting");
        assert_eq!(
            last.ended_at.unwrap() - last.started_at,
            chrono::Duration::seconds(21729)
        );
    }

    #[test]
    fn missing_pieces_yield_none() {
        assert!(parse(r#"{"data":{"user":{"videos":{"edges":[]}}}}"#).is_none());
        assert!(parse(r#"{"data":{"user":null}}"#).is_none());
        assert!(parse(r#"{}"#).is_none());
    }

    #[test]
    fn absent_game_is_empty() {
        let json = r#"{"data":{"user":{"videos":{"edges":[{"node":{
            "title":"t","createdAt":"2026-07-02T13:00:40Z","lengthSeconds":60}}]}}}}"#;
        assert_eq!(parse(json).unwrap().game, "");
    }
}
