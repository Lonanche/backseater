//! The official YouTube Data API live-chat feed (`liveChatMessages.streamList`),
//! used in chats the logged-in account owns or moderates.
//!
//! Compared with the anonymous InnerTube reader it carries the Data API's own
//! message ids (so delete targets exactly the clicked message) and real
//! ban/timeout events with the acting moderator. It lacks custom emoji images,
//! membership badges and name colors, which is why it isn't used everywhere.
//!
//! `streamList` is a server-streaming **gRPC** call (documented only in gRPC
//! form). Rather than pull in a gRPC framework for one RPC, it's spoken by hand
//! over `reqwest`'s HTTP/2: POST the length-prefixed protobuf request, then read
//! length-prefixed `LiveChatMessageListResponse` frames from the body. The
//! messages below are a hand-written subset of Google's `stream_list.proto`
//! (field numbers copied from it); prost skips every field not listed.
//!
//! YouTube ends each call cleanly after about 10 seconds (gRPC status 0 plus a
//! resume token) and expects the client to call again, so a clean end
//! reconnects at once from the last `next_page_token` — no history replay. Each
//! call is billed (documented as 5 units; still being measured), so a dropped
//! or failing connection backs off and eventually gives up.

use std::time::{Duration, Instant};

use bks_core::{Author, Message, MessageElement, Platform};
use bks_platform::{ChatEvent, ChatSink, EventDetails, EventKind};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt as _;
use prost::Message as _;

const ENDPOINT: &str =
    "https://youtube.googleapis.com/youtube.api.v3.V3DataLiveChatMessageService/StreamList";
/// A connection that lasted at least this long resets the reconnect backoff.
const HEALTHY_CONNECTION: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Consecutive failed connections before giving up (each one may cost quota).
const MAX_FAILURES: u32 = 6;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ListRequest {
    #[prost(string, optional, tag = "1")]
    pub live_chat_id: Option<String>,
    #[prost(string, optional, tag = "99")]
    pub page_token: Option<String>,
    #[prost(string, repeated, tag = "100")]
    pub part: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ListResponse {
    #[prost(string, optional, tag = "2")]
    pub offline_at: Option<String>,
    #[prost(string, optional, tag = "100602")]
    pub next_page_token: Option<String>,
    #[prost(message, repeated, tag = "1007")]
    pub items: Vec<ChatItem>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ChatItem {
    #[prost(string, optional, tag = "101")]
    pub id: Option<String>,
    #[prost(message, optional, tag = "2")]
    pub snippet: Option<Snippet>,
    #[prost(message, optional, tag = "3")]
    pub author_details: Option<AuthorDetails>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct AuthorDetails {
    #[prost(string, optional, tag = "10101")]
    pub channel_id: Option<String>,
    #[prost(string, optional, tag = "103")]
    pub display_name: Option<String>,
    #[prost(bool, optional, tag = "5")]
    pub is_chat_owner: Option<bool>,
    #[prost(bool, optional, tag = "6")]
    pub is_chat_sponsor: Option<bool>,
    #[prost(bool, optional, tag = "7")]
    pub is_chat_moderator: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Snippet {
    #[prost(int32, optional, tag = "1")]
    pub kind: Option<i32>,
    #[prost(string, optional, tag = "4")]
    pub published_at: Option<String>,
    #[prost(string, optional, tag = "16")]
    pub display_message: Option<String>,
    #[prost(message, optional, tag = "19")]
    pub text_message_details: Option<TextDetails>,
    #[prost(message, optional, tag = "22")]
    pub user_banned_details: Option<BannedDetails>,
    #[prost(message, optional, tag = "27")]
    pub super_chat_details: Option<SuperChatDetails>,
    #[prost(message, optional, tag = "28")]
    pub super_sticker_details: Option<SuperStickerDetails>,
    #[prost(message, optional, tag = "29")]
    pub new_sponsor_details: Option<NewSponsorDetails>,
    #[prost(message, optional, tag = "30")]
    pub member_milestone_chat_details: Option<MilestoneDetails>,
    #[prost(message, optional, tag = "31")]
    pub membership_gifting_details: Option<GiftingDetails>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct TextDetails {
    #[prost(string, optional, tag = "1")]
    pub message_text: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct BannedDetails {
    #[prost(message, optional, tag = "1")]
    pub banned_user_details: Option<ChannelProfile>,
    #[prost(int32, optional, tag = "2")]
    pub ban_type: Option<i32>,
    #[prost(uint64, optional, tag = "4")]
    pub ban_duration_seconds: Option<u64>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ChannelProfile {
    #[prost(string, optional, tag = "101")]
    pub channel_id: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub display_name: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SuperChatDetails {
    #[prost(string, optional, tag = "3")]
    pub amount_display_string: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub user_comment: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SuperStickerDetails {
    #[prost(string, optional, tag = "3")]
    pub amount_display_string: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct NewSponsorDetails {
    #[prost(string, optional, tag = "1")]
    pub member_level_name: Option<String>,
    #[prost(bool, optional, tag = "2")]
    pub is_upgrade: Option<bool>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MilestoneDetails {
    #[prost(string, optional, tag = "1")]
    pub member_level_name: Option<String>,
    #[prost(uint32, optional, tag = "2")]
    pub member_month: Option<u32>,
    #[prost(string, optional, tag = "3")]
    pub user_comment: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct GiftingDetails {
    #[prost(int32, optional, tag = "1")]
    pub gift_memberships_count: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub gift_memberships_level_name: Option<String>,
}

// `LiveChatMessageSnippet.TypeWrapper.Type` values.
const TEXT_MESSAGE_EVENT: i32 = 1;
const TOMBSTONE: i32 = 2;
const CHAT_ENDED_EVENT: i32 = 4;
const NEW_SPONSOR_EVENT: i32 = 7;
const USER_BANNED_EVENT: i32 = 10;
const SUPER_CHAT_EVENT: i32 = 15;
const SUPER_STICKER_EVENT: i32 = 16;
const MEMBER_MILESTONE_CHAT_EVENT: i32 = 17;
const MEMBERSHIP_GIFTING_EVENT: i32 = 18;
// `BanTypeWrapper.BanType`.
const BAN_TEMPORARY: i32 = 2;

/// gRPC status codes this reader reacts to.
const GRPC_NOT_FOUND: u32 = 5;
const GRPC_PERMISSION_DENIED: u32 = 7;
const GRPC_RESOURCE_EXHAUSTED: u32 = 8;
const GRPC_FAILED_PRECONDITION: u32 = 9;
const GRPC_UNAUTHENTICATED: u32 = 16;

/// Why [`stream_chat`] stopped without an error.
pub(crate) enum Stopped {
    /// The live chat ended (stream offline / chat closed).
    ChatEnded,
    /// The receiving side went away.
    Closed,
}

/// A failed connection, classified by what the reader should do next.
enum ConnectError {
    /// The access token was rejected; refresh it and retry right away.
    Unauthenticated,
    /// The chat is gone or not open; stop as if it ended.
    ChatGone,
    /// Retrying can't help (no permission, quota used up, endpoint missing).
    Fatal(anyhow::Error),
    /// Possibly temporary (network, server hiccup); retry with backoff.
    Retry(anyhow::Error),
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // In the full app another dependency turns on reqwest's native-tls
            // backend, which becomes the default and (built without its ALPN
            // feature) never offers "h2" — Google then answers in HTTP/1.1 and
            // the HTTP/2 framing fails ("frame with invalid size"). rustls
            // offers h2.
            .use_rustls_tls()
            .http2_prior_knowledge()
            .connect_timeout(Duration::from_secs(10))
            // The stream is long-lived and may be quiet for minutes, so there is
            // no read timeout; HTTP/2 pings detect a dead connection instead.
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(20))
            .http2_keep_alive_while_idle(true)
            .no_gzip()
            .build()
            .expect("building youtube grpc client")
    })
}

/// Streams `live_chat_id`'s messages into `tx` until the chat ends, the
/// receiver closes, or an unrecoverable error. Messages published at or before
/// `cutoff` are dropped (the InnerTube reader already showed them); with no
/// cutoff, the first batch is shown as history.
pub(crate) async fn stream_chat(
    actions: &crate::YoutubeActions,
    source: &str,
    live_chat_id: &str,
    tx: &ChatSink,
    cutoff: Option<DateTime<Utc>>,
) -> anyhow::Result<Stopped> {
    let mut state = StreamState {
        page_token: None,
        first_batch: true,
        cutoff,
    };
    let mut failures = 0u32;
    let mut backoff = Duration::from_secs(1);
    let mut refreshed = false;
    // Why the previous connection ended, for the give-up message.
    let mut last_problem: String;
    loop {
        if tx.is_closed() {
            return Ok(Stopped::Closed);
        }
        let token = actions.fresh_token().await?;
        let started = Instant::now();
        match connect(&token, live_chat_id, &mut state, source, tx).await {
            Ok(Pumped::ChatEnded) => return Ok(Stopped::ChatEnded),
            Ok(Pumped::Closed) => return Ok(Stopped::Closed),
            Ok(Pumped::Rotated) => {
                refreshed = false;
                failures = 0;
                backoff = Duration::from_secs(1);
                continue;
            }
            Ok(Pumped::Disconnected) => {
                refreshed = false;
                tracing::debug!(
                    "youtube official chat for {source} dropped after {:?}",
                    started.elapsed()
                );
                if started.elapsed() >= HEALTHY_CONNECTION {
                    failures = 0;
                    backoff = Duration::from_secs(1);
                    continue;
                }
                last_problem = "the stream closed right after opening".into();
            }
            Err(ConnectError::Unauthenticated) if !refreshed => {
                refreshed = true;
                actions.force_refresh(&token).await?;
                continue;
            }
            Err(ConnectError::Unauthenticated) => {
                anyhow::bail!("YouTube rejected the login for live chat")
            }
            Err(ConnectError::ChatGone) => return Ok(Stopped::ChatEnded),
            Err(ConnectError::Fatal(err)) => return Err(err),
            Err(ConnectError::Retry(err)) => {
                tracing::debug!("youtube official chat for {source} failed: {err:#}");
                last_problem = format!("{err:#}");
            }
        }
        failures += 1;
        if failures >= MAX_FAILURES {
            anyhow::bail!("YouTube live chat kept disconnecting ({last_problem})");
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

struct StreamState {
    page_token: Option<String>,
    first_batch: bool,
    cutoff: Option<DateTime<Utc>>,
}

enum Pumped {
    ChatEnded,
    Closed,
    /// YouTube finished the call normally (status 0); call again right away.
    Rotated,
    /// The connection dropped without a status; resume with backoff.
    Disconnected,
}

/// Official chat calls made this run, logged so they can be compared with the
/// quota page.
static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One `StreamList` connection: sends the request, then forwards every
/// response frame until the stream closes.
async fn connect(
    token: &str,
    live_chat_id: &str,
    state: &mut StreamState,
    source: &str,
    tx: &ChatSink,
) -> Result<Pumped, ConnectError> {
    let request = ListRequest {
        live_chat_id: Some(live_chat_id.to_string()),
        page_token: state.page_token.clone(),
        part: vec!["id".into(), "snippet".into(), "authorDetails".into()],
    };
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tracing::debug!("youtube official chat for {source}: call #{call} this run");
    let resp = http_client()
        .post(ENDPOINT)
        .header(reqwest::header::CONTENT_TYPE, "application/grpc")
        .header("te", "trailers")
        .bearer_auth(token)
        .body(frame(&request.encode_to_vec()))
        .send()
        .await
        .map_err(|err| ConnectError::Retry(err.into()))?;

    // A request that fails before any message is a "trailers-only" response:
    // the gRPC status rides in the headers.
    tracing::debug!(
        "youtube official chat for {source}: HTTP {} {:?}, headers {:?}",
        resp.status(),
        resp.version(),
        resp.headers()
    );
    if let Some(code) = header_u32(resp.headers(), "grpc-status") {
        if code == 0 {
            return Ok(Pumped::Rotated);
        }
        let message = resp
            .headers()
            .get("grpc-message")
            .and_then(|v| v.to_str().ok())
            .map(percent_decode)
            .unwrap_or_default();
        return Err(classify(code, &message));
    }
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let err = anyhow::anyhow!(
            "YouTube live chat returned HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        );
        return Err(match status.as_u16() {
            401 => ConnectError::Unauthenticated,
            400 | 403 | 404 | 501 => ConnectError::Fatal(err),
            _ => ConnectError::Retry(err),
        });
    }

    // Read HTTP/2 frames directly (not `Response::chunk`): gRPC reports an
    // error that happens after the stream started in the trailers.
    let mut body = reqwest::Body::from(resp);
    let mut buf = Vec::new();
    let mut responses = 0u32;
    loop {
        let frame = match body.frame().await {
            Some(Ok(frame)) => frame,
            None => {
                tracing::debug!(
                    "youtube official chat for {source}: body ended without trailers after {responses} responses"
                );
                return Ok(Pumped::Disconnected);
            }
            Some(Err(err)) => {
                tracing::debug!("youtube official chat stream for {source} dropped: {err:#}");
                return Ok(Pumped::Disconnected);
            }
        };
        let chunk = match frame.into_data() {
            Ok(chunk) => chunk,
            Err(frame) => {
                let Some(trailers) = frame.trailers_ref() else {
                    continue;
                };
                tracing::debug!(
                    "youtube official chat for {source}: trailers after {responses} responses: {trailers:?}"
                );
                return match header_u32(trailers, "grpc-status") {
                    Some(code) if code != 0 => {
                        let message = trailers
                            .get("grpc-message")
                            .and_then(|v| v.to_str().ok())
                            .map(percent_decode)
                            .unwrap_or_default();
                        Err(classify(code, &message))
                    }
                    Some(_) => Ok(Pumped::Rotated),
                    None => Ok(Pumped::Disconnected),
                };
            }
        };
        buf.extend_from_slice(&chunk);
        while let Some(message) = take_frame(&mut buf).map_err(ConnectError::Fatal)? {
            let response = ListResponse::decode(message.as_slice()).map_err(|err| {
                ConnectError::Fatal(anyhow::anyhow!("unreadable YouTube live chat data: {err}"))
            })?;
            responses += 1;
            tracing::debug!(
                "youtube official chat for {source}: response {responses} with {} items, next page token: {}, offline_at: {:?}",
                response.items.len(),
                response.next_page_token.as_deref().is_some_and(|t| !t.is_empty()),
                response.offline_at
            );
            if let Some(stop) = forward(response, state, source, tx).await {
                return Ok(stop);
            }
        }
    }
}

/// Sends one response's events; `Some` when the stream should stop.
async fn forward(
    response: ListResponse,
    state: &mut StreamState,
    source: &str,
    tx: &ChatSink,
) -> Option<Pumped> {
    if let Some(next) = response.next_page_token.filter(|t| !t.is_empty()) {
        state.page_token = Some(next);
    }
    let history = std::mem::take(&mut state.first_batch) && state.cutoff.is_none();
    let cutoff = if history { None } else { state.cutoff };
    let mut ended = response.offline_at.is_some();
    for item in &response.items {
        match convert(source, item, history, cutoff) {
            Converted::Events(events) => {
                for event in events {
                    if tx.send(event).await.is_err() {
                        return Some(Pumped::Closed);
                    }
                }
            }
            Converted::ChatEnded => ended = true,
            Converted::Nothing => {}
        }
    }
    ended.then_some(Pumped::ChatEnded)
}

fn classify(code: u32, message: &str) -> ConnectError {
    let err = |what: &str| anyhow::anyhow!("{what} (gRPC {code}: {message})");
    match code {
        GRPC_UNAUTHENTICATED => ConnectError::Unauthenticated,
        GRPC_NOT_FOUND | GRPC_FAILED_PRECONDITION => ConnectError::ChatGone,
        GRPC_PERMISSION_DENIED => {
            ConnectError::Fatal(err("YouTube won't let this account read this chat"))
        }
        GRPC_RESOURCE_EXHAUSTED => ConnectError::Fatal(err(
            "Backseater's shared daily YouTube quota is used up (it resets at midnight Pacific time)",
        )),
        // Cancelled, unknown, deadline exceeded, aborted, internal, unavailable:
        // worth another try.
        1 | 2 | 4 | 10 | 13 | 14 => ConnectError::Retry(err("YouTube live chat error")),
        // Invalid argument, unimplemented and the rest won't fix themselves.
        _ => ConnectError::Fatal(err("YouTube refused the live chat request")),
    }
}

/// A gRPC message frame: an uncompressed flag byte, the big-endian length,
/// then the protobuf bytes.
fn frame(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 5);
    out.push(0);
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// Removes one complete frame from the front of `buf`, if it holds one.
fn take_frame(buf: &mut Vec<u8>) -> anyhow::Result<Option<Vec<u8>>> {
    if buf.len() < 5 {
        return Ok(None);
    }
    anyhow::ensure!(
        buf[0] == 0,
        "compressed YouTube live chat data isn't supported"
    );
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    anyhow::ensure!(
        len <= 16 * 1024 * 1024,
        "oversized YouTube live chat message"
    );
    if buf.len() < 5 + len {
        return Ok(None);
    }
    let message = buf[5..5 + len].to_vec();
    buf.drain(..5 + len);
    Ok(Some(message))
}

fn header_u32(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u32> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Decodes the `%XX` escapes gRPC uses in `grpc-message`.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

enum Converted {
    Events(Vec<ChatEvent>),
    ChatEnded,
    Nothing,
}

/// Turns one official chat item into chat events (recording messages for the
/// moderation actions on the way).
fn convert(
    source: &str,
    item: &ChatItem,
    history: bool,
    cutoff: Option<DateTime<Utc>>,
) -> Converted {
    let Some(snippet) = &item.snippet else {
        return Converted::Nothing;
    };
    let kind = snippet.kind.unwrap_or_default();
    if kind == CHAT_ENDED_EVENT {
        return Converted::ChatEnded;
    }
    let timestamp = snippet
        .published_at
        .as_deref()
        .and_then(bks_core::parse_rfc3339)
        .unwrap_or_else(Utc::now);
    if cutoff.is_some_and(|cutoff| timestamp <= cutoff) {
        return Converted::Nothing;
    }
    let id = item.id.clone().unwrap_or_default();
    let author = item.author_details.clone().unwrap_or_default();
    let name = display_name(&author);
    let events = match kind {
        TEXT_MESSAGE_EVENT => {
            let text = snippet
                .text_message_details
                .as_ref()
                .and_then(|d| d.message_text.clone())
                .or_else(|| snippet.display_message.clone())
                .unwrap_or_default();
            match message(source, &id, &author, &text, timestamp, history) {
                Some(msg) => {
                    crate::seen::record_official(&msg, roles(&author));
                    vec![ChatEvent::Message(Box::new(msg))]
                }
                None => Vec::new(),
            }
        }
        TOMBSTONE if !id.is_empty() => vec![ChatEvent::DeleteMessage {
            platform: Platform::YouTube,
            message_id: id,
        }],
        USER_BANNED_EVENT => banned(snippet, &name, timestamp, history),
        SUPER_CHAT_EVENT => {
            let details = snippet.super_chat_details.clone().unwrap_or_default();
            let amount = details.amount_display_string.unwrap_or_default();
            let comment = details.user_comment.unwrap_or_default();
            let attached = message(source, &id, &author, &comment, timestamp, history);
            event(
                EventKind::Bits,
                &name,
                format!("sent {amount}"),
                timestamp,
                attached,
                history,
            )
        }
        SUPER_STICKER_EVENT => {
            let details = snippet.super_sticker_details.clone().unwrap_or_default();
            let amount = details.amount_display_string.unwrap_or_default();
            event(
                EventKind::Bits,
                &name,
                format!("sent a Super Sticker ({amount})"),
                timestamp,
                None,
                history,
            )
        }
        NEW_SPONSOR_EVENT => {
            let details = snippet.new_sponsor_details.clone().unwrap_or_default();
            let level = details.member_level_name.unwrap_or_default();
            let detail = match (details.is_upgrade.unwrap_or(false), level.is_empty()) {
                (true, false) => format!("upgraded to {level}"),
                (_, false) => format!("became a member ({level})"),
                _ => "became a member".to_string(),
            };
            event(EventKind::Sub, &name, detail, timestamp, None, history)
        }
        MEMBER_MILESTONE_CHAT_EVENT => {
            let details = snippet
                .member_milestone_chat_details
                .clone()
                .unwrap_or_default();
            let months = details.member_month.unwrap_or_default();
            let comment = details.user_comment.unwrap_or_default();
            let attached = message(source, &id, &author, &comment, timestamp, history);
            event(
                EventKind::Sub,
                &name,
                format!("member for {months} months"),
                timestamp,
                attached,
                history,
            )
        }
        MEMBERSHIP_GIFTING_EVENT => {
            let details = snippet
                .membership_gifting_details
                .clone()
                .unwrap_or_default();
            let count = details.gift_memberships_count.unwrap_or_default().max(0) as u32;
            let mut events = event(
                EventKind::Gift,
                &name,
                format!("gifted {count} memberships"),
                timestamp,
                None,
                history,
            );
            if let Some(ChatEvent::Event { details, .. }) = events.first_mut() {
                details.gift_count = Some(count);
            }
            events
        }
        _ => Vec::new(),
    };
    if events.is_empty() {
        Converted::Nothing
    } else {
        Converted::Events(events)
    }
}

fn display_name(author: &AuthorDetails) -> String {
    bks_core::normalize_username(author.display_name.as_deref().unwrap_or_default()).to_string()
}

fn roles(author: &AuthorDetails) -> crate::seen::AuthorRoles {
    crate::seen::AuthorRoles {
        moderator: author.is_chat_moderator.unwrap_or(false),
        owner: author.is_chat_owner.unwrap_or(false),
    }
}

/// A chat line in the same shape as the InnerTube builder's (plain text: the
/// official feed has no emoji images, membership badges or name colors; members
/// get YouTube's member green).
fn message(
    source: &str,
    id: &str,
    author: &AuthorDetails,
    text: &str,
    timestamp: DateTime<Utc>,
    historical: bool,
) -> Option<Message> {
    let display = display_name(author);
    if id.is_empty() || display.is_empty() || text.trim().is_empty() {
        return None;
    }
    let color = author
        .is_chat_sponsor
        .unwrap_or(false)
        .then_some(crate::builder::MEMBER_NAME_COLOR);
    let elements = bks_core::mentionize(bks_core::linkify(vec![MessageElement::Text {
        text: text.to_string(),
        color,
    }]));
    Some(Message {
        id: id.to_string(),
        platform: Platform::YouTube,
        channel: source.to_string(),
        timestamp,
        author: Author {
            login: display.to_lowercase(),
            display_name: display,
            color,
            badges: Vec::new(),
            paint: None,
            user_id: author.channel_id.clone().unwrap_or_default(),
        },
        elements,
        raw_text: text.to_string(),
        reply: None,
        first_message: false,
        highlighted: false,
        historical,
        reward_id: None,
        source: None,
    })
}

fn event(
    kind: EventKind,
    actor: &str,
    detail: String,
    timestamp: DateTime<Utc>,
    attached: Option<Message>,
    historical: bool,
) -> Vec<ChatEvent> {
    if actor.is_empty() {
        return Vec::new();
    }
    vec![ChatEvent::Event {
        platform: Platform::YouTube,
        kind,
        text: format!("{actor} {detail}"),
        timestamp,
        message: attached.map(Box::new),
        details: EventDetails {
            actor: Some(actor.to_string()),
            compact: Some(detail),
            historical,
            ..Default::default()
        },
    }]
}

/// A ban or timeout: strike the user's lines and say who did it. On YouTube a
/// permanent ban hides the user: they can still watch and type, but nobody
/// else sees their messages.
fn banned(
    snippet: &Snippet,
    moderator: &str,
    timestamp: DateTime<Utc>,
    history: bool,
) -> Vec<ChatEvent> {
    let Some(details) = &snippet.user_banned_details else {
        return Vec::new();
    };
    let target = details
        .banned_user_details
        .as_ref()
        .and_then(|p| p.display_name.as_deref())
        .map(bks_core::normalize_username)
        .unwrap_or_default()
        .to_string();
    if target.is_empty() {
        return Vec::new();
    }
    let mut events = vec![ChatEvent::ClearChat {
        platform: Platform::YouTube,
        user: Some(target.to_lowercase()),
        historical: history,
        timestamp: Some(timestamp),
    }];
    if !history {
        let by = if moderator.is_empty() {
            "A moderator"
        } else {
            moderator
        };
        let notice = if details.ban_type == Some(BAN_TEMPORARY) {
            let secs = details.ban_duration_seconds.unwrap_or(300);
            format!(
                "{by} timed out {target} for {}",
                bks_core::format_duration(secs)
            )
        } else {
            format!("{by} hid {target} from the chat")
        };
        events.push(ChatEvent::Notice(notice));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_item(id: &str, name: &str, text: &str, at: &str) -> ChatItem {
        ChatItem {
            id: Some(id.into()),
            snippet: Some(Snippet {
                kind: Some(TEXT_MESSAGE_EVENT),
                published_at: Some(at.into()),
                text_message_details: Some(TextDetails {
                    message_text: Some(text.into()),
                }),
                ..Default::default()
            }),
            author_details: Some(AuthorDetails {
                channel_id: Some("UCauthor".into()),
                display_name: Some(name.into()),
                is_chat_moderator: Some(true),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn frames_round_trip_and_wait_for_whole_messages() {
        let response = ListResponse {
            next_page_token: Some("next".into()),
            items: vec![text_item("LCC.1", "@Viewer", "hi", "2026-11-11T12:00:00Z")],
            ..Default::default()
        };
        let framed = frame(&response.encode_to_vec());
        let mut buf = framed[..framed.len() - 1].to_vec();
        assert!(take_frame(&mut buf).unwrap().is_none());
        buf.push(*framed.last().unwrap());
        let message = take_frame(&mut buf).unwrap().unwrap();
        assert!(buf.is_empty());
        assert_eq!(ListResponse::decode(message.as_slice()).unwrap(), response);
    }

    #[test]
    fn text_messages_keep_the_official_id_and_normalize_the_name() {
        let item = text_item("LCC.1", "@Viewer", "hello", "2026-11-11T12:00:00Z");
        let Converted::Events(events) = convert("@chan", &item, false, None) else {
            panic!("expected a message");
        };
        let ChatEvent::Message(msg) = &events[0] else {
            panic!("expected a message");
        };
        assert_eq!(msg.id, "LCC.1");
        assert_eq!(msg.author.login, "viewer");
        assert_eq!(msg.author.user_id, "UCauthor");
        assert_eq!(msg.raw_text, "hello");
        assert_eq!(
            crate::seen::author_roles("@chan", "UCauthor"),
            Some(crate::seen::AuthorRoles {
                moderator: true,
                owner: false
            })
        );
    }

    #[test]
    fn messages_at_or_before_the_cutoff_are_dropped() {
        let cutoff = bks_core::parse_rfc3339("2026-11-11T12:00:00Z");
        let old = text_item("LCC.1", "a", "x", "2026-11-11T12:00:00Z");
        let new = text_item("LCC.2", "a", "y", "2026-11-11T12:00:01Z");
        assert!(matches!(
            convert("s", &old, false, cutoff),
            Converted::Nothing
        ));
        assert!(matches!(
            convert("s", &new, false, cutoff),
            Converted::Events(_)
        ));
    }

    #[test]
    fn bans_strike_the_user_and_name_the_moderator() {
        let item = |ban_type, secs| ChatItem {
            id: Some("LCC.b".into()),
            snippet: Some(Snippet {
                kind: Some(USER_BANNED_EVENT),
                published_at: Some("2026-11-11T12:00:00Z".into()),
                user_banned_details: Some(BannedDetails {
                    banned_user_details: Some(ChannelProfile {
                        channel_id: Some("UCtarget".into()),
                        display_name: Some("@Spammer".into()),
                    }),
                    ban_type: Some(ban_type),
                    ban_duration_seconds: secs,
                }),
                ..Default::default()
            }),
            author_details: Some(AuthorDetails {
                display_name: Some("@Mod".into()),
                ..Default::default()
            }),
        };
        let Converted::Events(events) = convert("s", &item(BAN_TEMPORARY, Some(600)), false, None)
        else {
            panic!("expected events");
        };
        assert!(matches!(&events[0], ChatEvent::ClearChat { user: Some(u), .. } if u == "spammer"));
        assert!(matches!(&events[1], ChatEvent::Notice(n) if n == "Mod timed out Spammer for 10m"));
        let Converted::Events(events) = convert("s", &item(1, None), false, None) else {
            panic!("expected events");
        };
        assert!(matches!(&events[1], ChatEvent::Notice(n) if n == "Mod hid Spammer from the chat"));
    }

    #[test]
    fn chat_end_and_errors_are_classified() {
        let item = ChatItem {
            snippet: Some(Snippet {
                kind: Some(CHAT_ENDED_EVENT),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(matches!(
            convert("s", &item, false, None),
            Converted::ChatEnded
        ));
        assert!(matches!(classify(16, ""), ConnectError::Unauthenticated));
        assert!(matches!(classify(9, ""), ConnectError::ChatGone));
        assert!(matches!(classify(8, ""), ConnectError::Fatal(_)));
        assert!(matches!(classify(14, ""), ConnectError::Retry(_)));
        assert!(matches!(classify(12, ""), ConnectError::Fatal(_)));
        assert!(matches!(classify(3, ""), ConnectError::Fatal(_)));
        assert_eq!(percent_decode("quota%20exceeded"), "quota exceeded");
    }
}
