//! App-wide **shared** EventSub WebSocket, multiplexing every tab's moderator
//! feed onto one connection.
//!
//! Twitch caps EventSub at **3 WebSocket connections with enabled subscriptions
//! per (client id, user id)** (see the docs on Handling WebSocket Events). The
//! old design opened one socket *per Twitch tab*, so a user modding 4+ channels —
//! or a startup burst of tabs racing for slots — got a 429 "number of websocket
//! transports limit exceeded", and the per-tab reconnect loop then retried
//! forever, each retry opening yet another socket that made it worse.
//!
//! This manager instead owns a **single** socket for the logged-in user. Tabs
//! [`register`] a `(broadcaster_id, sink)` and get back a [`Registration`] guard;
//! the manager creates that channel's subscriptions on the shared session and
//! routes each incoming notification to the right tab's sink by
//! `broadcaster_user_id`. One socket holds up to 300 subscriptions (~100 channels
//! at 3 subs each), far above any realistic tab count. Dropping the guard removes
//! the channel and best-effort deletes its subscriptions so the slot frees up.
//!
//! A single background task runs the socket, reconnecting with backoff. On each
//! (re)connect it (re)subscribes every currently-registered channel on the fresh
//! session. Registering while connected subscribes immediately; the whole thing
//! is driven by an internal command channel so registration and the socket loop
//! don't share locks across await points.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use bks_core::Platform;
use bks_platform::{ChatEvent, ChatSink};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::eventsub::{
    delete_subscription, is_transport_limit, notification_events, subscribe, EventsubAuth, Frame,
    SubResult, EVENTSUB_URL,
};

/// Commands the socket task reacts to.
enum Command {
    /// Add a channel's feed (subscribe on the live session if there is one).
    Register {
        broadcaster_id: String,
        sink: ChatSink,
    },
    /// Remove a channel's feed and delete its subscriptions.
    Unregister { broadcaster_id: String },
}

/// A live channel registration on the shared socket. The channel's feed stays up
/// as long as this guard lives; dropping it unregisters the channel (which also
/// tells the UI to restore its generic moderation notices).
pub struct Registration {
    broadcaster_id: String,
    manager: Arc<ManagerInner>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.manager.send(Command::Unregister {
            broadcaster_id: std::mem::take(&mut self.broadcaster_id),
        });
    }
}

struct ManagerInner {
    /// Sends commands to the running socket task. `None` until first `register`.
    tx: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    /// The auth the socket task is bound to. A different auth (re-login) resets
    /// the manager so the new user token's socket replaces the old one.
    auth_key: Mutex<Option<AuthKey>>,
}

/// Identifies the logged-in user a socket is bound to. A change here (new login)
/// means the running task must be torn down and rebuilt.
#[derive(Clone, PartialEq)]
struct AuthKey {
    client_id: String,
    user_id: String,
    token: String,
}

impl ManagerInner {
    fn send(&self, cmd: Command) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(cmd);
        }
    }
}

/// The process-wide manager singleton.
fn manager() -> &'static Arc<ManagerInner> {
    static M: OnceLock<Arc<ManagerInner>> = OnceLock::new();
    M.get_or_init(|| {
        Arc::new(ManagerInner {
            tx: Mutex::new(None),
            auth_key: Mutex::new(None),
        })
    })
}

/// Registers `broadcaster_id`'s moderator feed on the app-wide shared EventSub
/// socket, forwarding its notifications into `sink`. Returns a guard that keeps
/// the feed alive until dropped, or `None` when the token can't power the feed at
/// all (no scopes — nothing to connect for).
///
/// Must run on the tokio runtime (it spawns the socket task on first use).
pub fn register(auth: EventsubAuth, broadcaster_id: String, sink: ChatSink) -> Option<Registration> {
    if !auth.feed_available() {
        tracing::info!(
            "twitch token lacks the moderator-feed scopes; log out and back in to enable \
             rich moderation notices + AutoMod"
        );
        return None;
    }
    crate::pubsub::ensure_crypto_provider();

    let manager = manager();
    let key = AuthKey {
        client_id: auth.client_id.clone(),
        user_id: auth.user_id.clone(),
        token: auth.token.clone(),
    };

    // Bind (or rebind on a login change) the socket task to this auth. A rebind
    // drops the old command sender, which ends the old task; a fresh one starts.
    {
        let mut cur = manager.auth_key.lock().unwrap();
        let mut tx_slot = manager.tx.lock().unwrap();
        if cur.as_ref() != Some(&key) || tx_slot.is_none() {
            let (tx, rx) = mpsc::unbounded_channel::<Command>();
            *tx_slot = Some(tx);
            *cur = Some(key.clone());
            let auth = auth.clone();
            // Runs on the ambient tokio runtime — `register` is always called
            // from a task on the app's runtime (the bridge's feed spawner).
            tokio::spawn(async move { socket_task(auth, rx).await });
        }
    }

    manager.send(Command::Register {
        broadcaster_id: broadcaster_id.clone(),
        sink,
    });

    Some(Registration {
        broadcaster_id,
        manager: manager.clone(),
    })
}

/// One channel's routing state on the shared socket.
struct Channel {
    sink: ChatSink,
    /// Subscription ids created for this channel on the *current* session (for
    /// deletion on unregister). Rebuilt on each reconnect.
    sub_ids: Vec<String>,
    /// Whether `channel.moderate` is active — gates the `ModFeed` on/off signal.
    moderate_active: bool,
}

/// Drives the shared socket for one auth until the command sender is dropped
/// (login change / manager reset). Reconnects with backoff internally.
async fn socket_task(auth: EventsubAuth, mut commands: mpsc::UnboundedReceiver<Command>) {
    // The set of registered channels survives reconnects; only the per-session
    // subscription ids are rebuilt each time.
    let mut channels: HashMap<String, Channel> = HashMap::new();
    let mut attempt: u32 = 0;

    loop {
        let started = std::time::Instant::now();
        let outcome = run_session(&auth, &mut channels, &mut commands).await;
        // Tell every channel's UI its feed went quiet (a reconnect will re-assert
        // it if the subscription comes back).
        for ch in channels.values_mut() {
            if ch.moderate_active {
                ch.moderate_active = false;
                let _ = ch.sink.send(ChatEvent::ModFeed {
                    platform: Platform::Twitch,
                    active: false,
                });
            }
            ch.sub_ids.clear();
        }
        match outcome {
            SessionOutcome::Shutdown => break, // command sender dropped — task retired
            SessionOutcome::TransportLimit => {
                // Opening another socket would make this worse. Something else is
                // holding the user's 3 EventSub slots (a stale session, another
                // client). Wait a long beat before retrying rather than hammering.
                tracing::warn!(
                    "twitch moderator feed: hit the 3-socket EventSub limit; \
                     retrying in 60s (the shared socket should keep us under it — \
                     a stale connection or another app may be holding slots)"
                );
                if sleep_or_shutdown(std::time::Duration::from_secs(60), &mut commands).await {
                    break;
                }
                attempt = 0;
            }
            SessionOutcome::Transient(err) => {
                tracing::warn!("twitch moderator feed: {err:#}; reconnecting");
                if started.elapsed() > std::time::Duration::from_secs(60) {
                    attempt = 0;
                }
                if sleep_or_shutdown(bks_core::reconnect_delay(attempt), &mut commands).await {
                    break;
                }
                attempt += 1;
            }
        }
    }

    // Best-effort: free the subscriptions we still hold so the user's slots clear.
    let client = crate::http::client();
    for ch in channels.values() {
        for id in &ch.sub_ids {
            delete_subscription(&client, &auth, id).await;
        }
    }
}

enum SessionOutcome {
    /// The command channel closed — the task should retire.
    Shutdown,
    /// Hit the 3-socket transport limit creating a subscription.
    TransportLimit,
    /// A transient failure (socket drop, keepalive miss, reconnect request).
    Transient(anyhow::Error),
}

/// Runs one WebSocket session: connect, read the welcome, subscribe every
/// registered channel, then pump notifications + handle register/unregister
/// commands until the socket dies or the command channel closes.
async fn run_session(
    auth: &EventsubAuth,
    channels: &mut HashMap<String, Channel>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> SessionOutcome {
    let (ws, _) = match tokio_tungstenite::connect_async(EVENTSUB_URL).await {
        Ok(ws) => ws,
        Err(err) => return SessionOutcome::Transient(anyhow::Error::new(err)),
    };
    let (mut write, mut read) = ws.split();

    // Await the welcome, answering protocol pings in the meantime.
    let (session_id, keepalive_secs) = match read_welcome(&mut write, &mut read).await {
        Ok(pair) => pair,
        Err(err) => return SessionOutcome::Transient(err),
    };

    let client = crate::http::client();
    // (Re)subscribe every registered channel on this fresh session.
    for (broadcaster_id, ch) in channels.iter_mut() {
        match subscribe_channel(&client, auth, &session_id, broadcaster_id).await {
            Ok((sub_ids, moderate_active)) => {
                ch.sub_ids = sub_ids;
                if moderate_active && !ch.moderate_active {
                    ch.moderate_active = true;
                    let _ = ch.sink.send(ChatEvent::ModFeed {
                        platform: Platform::Twitch,
                        active: true,
                    });
                }
            }
            Err(err) if is_transport_limit(&err) => return SessionOutcome::TransportLimit,
            Err(err) => return SessionOutcome::Transient(err),
        }
    }

    // Pump: notifications from the socket, plus register/unregister commands.
    let frame_timeout = std::time::Duration::from_secs(keepalive_secs + 15);
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                None => return SessionOutcome::Shutdown,
                Some(Command::Register { broadcaster_id, sink }) => {
                    // Subscribe the new channel on the live session right away.
                    let mut ch = Channel { sink, sub_ids: Vec::new(), moderate_active: false };
                    match subscribe_channel(&client, auth, &session_id, &broadcaster_id).await {
                        Ok((sub_ids, moderate_active)) => {
                            ch.sub_ids = sub_ids;
                            if moderate_active {
                                ch.moderate_active = true;
                                let _ = ch.sink.send(ChatEvent::ModFeed {
                                    platform: Platform::Twitch,
                                    active: true,
                                });
                            }
                            channels.insert(broadcaster_id, ch);
                        }
                        Err(err) if is_transport_limit(&err) => {
                            channels.insert(broadcaster_id, ch);
                            return SessionOutcome::TransportLimit;
                        }
                        Err(err) => {
                            channels.insert(broadcaster_id, ch);
                            return SessionOutcome::Transient(err);
                        }
                    }
                }
                Some(Command::Unregister { broadcaster_id }) => {
                    if let Some(ch) = channels.remove(&broadcaster_id) {
                        for id in ch.sub_ids {
                            delete_subscription(&client, auth, &id).await;
                        }
                    }
                }
            },
            frame = tokio::time::timeout(frame_timeout, read.next()) => {
                let frame = match frame {
                    Err(_) => return SessionOutcome::Transient(
                        anyhow::anyhow!("EventSub keepalive missed"),
                    ),
                    Ok(None) => return SessionOutcome::Transient(
                        anyhow::anyhow!("EventSub socket closed"),
                    ),
                    Ok(Some(Err(err))) => return SessionOutcome::Transient(
                        anyhow::Error::new(err).context("EventSub socket error"),
                    ),
                    Ok(Some(Ok(f))) => f,
                };
                match frame {
                    WsMessage::Text(t) => {
                        if let Some(outcome) = handle_frame(&t, channels) {
                            return outcome;
                        }
                    }
                    WsMessage::Ping(p) => { let _ = write.send(WsMessage::Pong(p)).await; }
                    WsMessage::Close(_) => return SessionOutcome::Transient(
                        anyhow::anyhow!("EventSub socket closed by Twitch"),
                    ),
                    _ => {}
                }
            }
        }
    }
}

/// Parses a notification/reconnect/revocation text frame, routing notifications
/// to the owning channel's sink. Returns `Some(outcome)` when the session should
/// end (Twitch moved the session), else `None` to keep pumping.
fn handle_frame(text: &str, channels: &mut HashMap<String, Channel>) -> Option<SessionOutcome> {
    let frame: Frame = serde_json::from_str(text).ok()?;
    match frame.metadata.message_type.as_str() {
        "notification" => {
            let sub = &frame.payload["subscription"];
            let sub_type = sub["type"].as_str().unwrap_or_default();
            // Route by the condition's broadcaster (the event object also carries
            // it, but the condition is always present and unambiguous).
            let broadcaster_id = sub["condition"]["broadcaster_user_id"]
                .as_str()
                .or_else(|| frame.payload["event"]["broadcaster_user_id"].as_str())
                .unwrap_or_default();
            if let Some(ch) = channels.get(broadcaster_id) {
                let event = &frame.payload["event"];
                for ev in notification_events(sub_type, event, Utc::now()) {
                    let _ = ch.sink.send(ev);
                }
            }
            None
        }
        // Twitch is moving the session to another edge; reconnect from scratch.
        "session_reconnect" => Some(SessionOutcome::Transient(anyhow::anyhow!(
            "EventSub session moved by Twitch"
        ))),
        "revocation" => {
            let sub_type = frame.payload["subscription"]["type"]
                .as_str()
                .unwrap_or_default();
            tracing::warn!("EventSub subscription revoked: {sub_type}");
            None
        }
        _ => None, // session_keepalive and anything future.
    }
}

/// Creates a channel's subscriptions on `session_id`, returning the created ids
/// and whether `channel.moderate` came up. A per-subscription normal decline
/// (401/403) just omits that subscription; a transport-limit / transient error
/// propagates so the caller reconnects or backs off.
async fn subscribe_channel(
    client: &reqwest::Client,
    auth: &EventsubAuth,
    session_id: &str,
    broadcaster_id: &str,
) -> anyhow::Result<(Vec<String>, bool)> {
    let mut sub_ids = Vec::new();
    let mut moderate_active = false;

    if auth.wants_moderate() {
        match subscribe(client, auth, session_id, broadcaster_id, "channel.moderate", "2").await? {
            SubResult::Created(id) => {
                sub_ids.push(id);
                moderate_active = true;
            }
            SubResult::Declined => {
                tracing::info!("not a moderator in this channel — rich moderation notices stay off")
            }
        }
    }
    if auth.wants_automod() {
        for sub_type in ["automod.message.hold", "automod.message.update"] {
            match subscribe(client, auth, session_id, broadcaster_id, sub_type, "2").await? {
                SubResult::Created(id) => sub_ids.push(id),
                SubResult::Declined => {}
            }
        }
    }
    if !sub_ids.is_empty() {
        tracing::info!(
            "twitch moderator feed active for {broadcaster_id} ({} subscriptions)",
            sub_ids.len()
        );
    }
    Ok((sub_ids, moderate_active))
}

/// Reads frames until the `session_welcome`, answering protocol pings. Returns
/// the session id and the welcome frame's keepalive cadence.
async fn read_welcome<W, R>(write: &mut W, read: &mut R) -> anyhow::Result<(String, u64)>
where
    W: SinkExt<WsMessage> + Unpin,
    R: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    use anyhow::{bail, Context};
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let frame = tokio::time::timeout_at(deadline, read.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for EventSub welcome"))?
            .context("EventSub socket closed before welcome")?
            .context("EventSub welcome error")?;
        match frame {
            WsMessage::Text(t) => {
                let frame: Frame = serde_json::from_str(&t).context("parsing EventSub welcome")?;
                if frame.metadata.message_type != "session_welcome" {
                    bail!(
                        "expected session_welcome, got {}",
                        frame.metadata.message_type
                    );
                }
                let session = &frame.payload["session"];
                let id = session["id"]
                    .as_str()
                    .context("EventSub welcome without session id")?
                    .to_string();
                let keepalive = session["keepalive_timeout_seconds"].as_u64().unwrap_or(10);
                return Ok((id, keepalive));
            }
            WsMessage::Ping(p) => {
                let _ = write.send(WsMessage::Pong(p)).await;
            }
            WsMessage::Close(_) => bail!("EventSub socket closed before welcome"),
            _ => {}
        }
    }
}

/// Sleeps for `dur` but wakes early (returning `true`) if the command channel
/// closes meanwhile, so a retired manager doesn't dawdle in a backoff sleep.
async fn sleep_or_shutdown(
    dur: std::time::Duration,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        cmd = commands.recv() => cmd.is_none(),
    }
}
