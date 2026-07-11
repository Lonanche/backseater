//! Resolving a single 7TV emote by id, for the click-a-link popup.
//!
//! Uses the public REST endpoint (`GET /v3/emotes/<id>`) — no auth needed. (The
//! authed GraphQL features for adding an emote to a set were removed: 7TV's login
//! can't be completed by a native app — its OAuth is server-pinned and the token
//! hand-off is locked to 7TV's own browser origins. See the project notes.)

use anyhow::{anyhow, Context};
use bks_core::Emote;
use serde::Deserialize;

use crate::http::shared_client;
use crate::seventv::{best_image_url, Host, Owner};

const EMOTE_URL: &str = "https://7tv.io/v3/emotes/";

/// Resolves a single 7TV emote by id via the public REST endpoint (no auth), for
/// populating the popup when a 7TV emote link is clicked. The id may be either
/// 7TV id format; the response normalizes it.
pub async fn fetch_emote(id: &str) -> anyhow::Result<Emote> {
    #[derive(Deserialize)]
    struct EmoteResponse {
        id: String,
        name: String,
        #[serde(default)]
        animated: bool,
        host: Host,
        #[serde(default)]
        owner: Option<Owner>,
    }

    let resp = shared_client()
        .get(format!("{EMOTE_URL}{id}"))
        .send()
        .await
        .context("fetching 7TV emote")?
        .error_for_status()
        .context("7TV emote not found")?;
    let e: EmoteResponse = resp.json().await.context("parsing 7TV emote")?;
    let url = best_image_url(&e.host, e.animated)
        .ok_or_else(|| anyhow!("emote has no renderable image"))?;
    Ok(Emote {
        id: e.id,
        name: e.name,
        url,
        animated: e.animated,
        tooltip: bks_core::EmoteTooltip {
            provider: "7TV".into(),
            author: e.owner.and_then(Owner::name),
        },
    })
}
