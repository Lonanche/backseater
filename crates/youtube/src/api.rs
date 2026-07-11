//! InnerTube plumbing for anonymous YouTube live-chat reads.
//!
//! YouTube's public web app talks to a private JSON API ("InnerTube") that needs
//! no API key or OAuth and has no quota — the same one the browser uses. Each
//! request carries an `INNERTUBE_API_KEY`, an `INNERTUBE_CLIENT_VERSION` and a
//! `visitorData` blob scraped from a bootstrap page ([`bootstrap`]), plus a
//! `context.client` object ([`client_context`]). We use it for the two calls the
//! live-chat read loop needs: `youtubei/v1/next` (initial continuation + live
//! metadata) and `youtubei/v1/live_chat/get_live_chat` (the polled message feed).
//!
//! Unlike Kick, YouTube's endpoints are not Cloudflare-fingerprint-gated, so a
//! plain rustls `reqwest` client passes — no browser-TLS emulation needed. We
//! still send browser-looking headers (UA, consent cookie) so responses match
//! what the web app gets.

use anyhow::Context;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};

/// A YouTube video watched by an anonymous browser to seed InnerTube — the
/// "Me at the zoo" embed. Its HTML carries the API key / client version /
/// visitor data we need for every later call.
const BOOTSTRAP_URL: &str = "https://www.youtube.com/embed/jNQXAC9IVRw";

pub const NEXT_URL: &str = "https://www.youtube.com/youtubei/v1/next?prettyPrint=false";
pub const GET_LIVE_CHAT_URL: &str =
    "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat?prettyPrint=false";
/// Channel/tab contents (used for the Streams tab's "last live" info).
pub const BROWSE_URL: &str = "https://www.youtube.com/youtubei/v1/browse?prettyPrint=false";
/// Video metadata — its microformat carries `liveBroadcastDetails` (the live
/// start time for the uptime readout, which the `next` response lacks).
pub const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
/// Resolves a vanity URL (an `@handle` page) to its `UC…` browse id.
pub const RESOLVE_URL_URL: &str =
    "https://www.youtube.com/youtubei/v1/navigation/resolve_url?prettyPrint=false";

/// One process-wide client, shared across every YouTube read (pools connections).
/// Browser-looking default headers are baked in so responses match the web app.
static CLIENT: Lazy<Client> = Lazy::new(|| {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"
            .parse()
            .unwrap(),
    );
    headers.insert(
        reqwest::header::ACCEPT_LANGUAGE,
        "en-US,en;q=0.9".parse().unwrap(),
    );
    // Skip Google's consent interstitial (which otherwise replaces the page HTML).
    headers.insert(
        reqwest::header::COOKIE,
        "CONSENT=YES+cb.20210328-17-p0.en+FX+111; SOCS=CAI"
            .parse()
            .unwrap(),
    );
    Client::builder()
        .default_headers(headers)
        // No default timeout otherwise — a stalled connection would hang the
        // poll loop forever. get_live_chat returns promptly (the poll *delay* is
        // client-side, from the response's timeoutMs), so 20s is generous.
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("building youtube reqwest client")
});

/// The shared client, for the sibling modules that GET raw HTML (resolution).
pub fn client() -> &'static Client {
    &CLIENT
}

/// The InnerTube session parameters scraped from the bootstrap page. Every API
/// call needs the key (URL param) + client version (context) and sends the
/// visitor id as a header.
#[derive(Clone, Debug)]
pub struct InnertubeContext {
    pub api_key: String,
    pub client_version: String,
    pub visitor_data: String,
}

impl InnertubeContext {
    /// Scrapes a fresh context from the bootstrap embed page. Errors if any of the
    /// three fields is missing (YouTube changed the page or served a consent wall).
    pub async fn bootstrap() -> anyhow::Result<Self> {
        let html = CLIENT
            .get(BOOTSTRAP_URL)
            .header(reqwest::header::REFERER, "https://www.youtube.com/")
            .send()
            .await
            .context("fetching YouTube bootstrap page")?
            .text()
            .await
            .context("reading YouTube bootstrap page")?;

        let api_key = extract(&html, &API_KEY_RE);
        let client_version = extract(&html, &CLIENT_VERSION_RE);
        let visitor_data = extract(&html, &VISITOR_DATA_RE);

        match (api_key, client_version, visitor_data) {
            (Some(api_key), Some(client_version), Some(visitor_data)) => Ok(Self {
                api_key,
                client_version,
                visitor_data,
            }),
            _ => anyhow::bail!("could not extract InnerTube context from YouTube bootstrap page"),
        }
    }

    /// The `context.client` object every InnerTube request wraps its payload in.
    fn client_context(&self) -> Value {
        json!({
            "clientName": "WEB",
            "clientVersion": self.client_version,
            "hl": "en",
            "gl": "US",
        })
    }

    /// POSTs an InnerTube request: merges `body` with the client context, attaches
    /// the API key, visitor id, and a watch-page referer, and returns the parsed
    /// JSON. `url` is one of [`NEXT_URL`] / [`GET_LIVE_CHAT_URL`].
    pub async fn post(
        &self,
        url: &str,
        referer_video_id: &str,
        body: Value,
    ) -> anyhow::Result<Value> {
        let mut root = json!({
            "context": { "client": self.client_context() },
        });
        // Merge caller fields (videoId / continuation) into the root object.
        if let (Some(root_obj), Some(body_obj)) = (root.as_object_mut(), body.as_object()) {
            for (k, v) in body_obj {
                root_obj.insert(k.clone(), v.clone());
            }
        }

        let url = format!("{url}&key={}", self.api_key);
        let resp = CLIENT
            .post(url)
            .header("X-Goog-Visitor-Id", &self.visitor_data)
            .header(
                reqwest::header::REFERER,
                format!("https://www.youtube.com/watch?v={referer_video_id}"),
            )
            .json(&root)
            .send()
            .await
            .context("posting InnerTube request")?;
        if !resp.status().is_success() {
            anyhow::bail!("InnerTube request returned {}", resp.status());
        }
        resp.json().await.context("parsing InnerTube response")
    }
}

/// Returns the first capture group of `re` in `text`, if any.
fn extract(text: &str, re: &Regex) -> Option<String> {
    re.captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .filter(|s| !s.is_empty())
}

static API_KEY_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""INNERTUBE_API_KEY":"([^"]+)""#).unwrap());
static CLIENT_VERSION_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""INNERTUBE_CLIENT_VERSION":"([^"]+)""#).unwrap());
static VISITOR_DATA_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""visitorData":"([^"]+)""#).unwrap());
