//! App-wide **shared** Twitch IRC connections, multiplexing every tab's channel
//! onto one pair of sockets.
//!
//! The app holds exactly **two** IRC connections regardless of how many channels
//! are open: one **read** connection that receives PRIVMSG/USERNOTICE/etc. for
//! every joined channel, and one **write** connection dedicated to sending (so a
//! flood of inbound chat can't delay outgoing messages). The old design here
//! opened one socket *per tab*, so a user with 15 authenticated Twitch tabs
//! logged in 15 times as the same account — risking Twitch's per-account
//! connection limits.
//!
//! This manager instead owns, per [`AuthKey`] (anonymous, or one specific
//! logged-in user), a **shared read client** and — when authenticated — a
//! **shared write client**. Tabs [`register`] a `(channel, sink)` and get back an
//! [`Registration`] guard; the manager JOINs the channel on the read socket and
//! routes each incoming message to the owning tab's sink by `#channel`. Dropping
//! the guard PARTs the channel. Sends and replies for a channel are handed to the
//! write client, which also echoes our own message back (Twitch doesn't).
//!
//! A background [`socket_task`] runs the connection(s), reconnecting with backoff.
//! On each fresh session it re-JOINs every registered channel through a
//! **leaky-bucket join limiter** (18 joins / 12.5s, under Twitch's rate
//! limit). Registration/sending is driven by an internal command
//! channel so nothing holds a lock across an await, exactly like
//! [`crate::eventsub_manager`].
//!
//! A new login (different credentials) rebinds the manager: the old task retires
//! (its sockets close) and a fresh one starts; the controller's login-watch
//! re-registers each tab, so channels migrate from the anonymous socket to the
//! authenticated one automatically.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use bks_core::Platform;
use bks_platform::{ChatEvent, ChatModes, ChatSink};
use tokio::sync::mpsc;

use crate::connector::{
    build_channel_meta, clearchat_event, privmsg_to_message, usernotice_event, SelfState,
    TwitchAuth,
};

/// JOIN leaky-bucket (Chatterino's values): at most 18 JOINs per 12.5s window
/// (Twitch rate-limits joins to ~20/10s; 18/12.5s stays comfortably under).
const JOIN_RATELIMIT_BUDGET: usize = 18;
const JOIN_RATELIMIT_COOLDOWN: std::time::Duration = std::time::Duration::from_millis(12_500);

/// Commands the socket task reacts to.
enum Command {
    /// Add a channel: JOIN it (on the live session, if any) and route its
    /// messages into `sink`.
    Register { channel: String, sink: ChatSink },
    /// Remove a channel: PART it and stop routing.
    Unregister { channel: String },
    /// Send a message (or reply) to a channel via the write client. The read
    /// connection delivers it back as a real PRIVMSG, so no local echo is made.
    Send {
        channel: String,
        text: String,
        reply_parent_id: Option<String>,
    },
}

/// A live channel registration on the shared IRC connection. The channel stays
/// joined as long as this guard lives; dropping it PARTs the channel.
pub struct Registration {
    channel: String,
    manager: Arc<ManagerInner>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.manager.send(Command::Unregister {
            channel: std::mem::take(&mut self.channel),
        });
    }
}

impl Registration {
    /// Enqueues a message to send on the shared write client for this channel.
    pub fn send(&self, text: String, reply_parent_id: Option<String>) {
        self.manager.send(Command::Send {
            channel: self.channel.clone(),
            text,
            reply_parent_id,
        });
    }
}

struct ManagerInner {
    /// Sends commands to the running socket task. `None` until the first
    /// `register` binds it.
    tx: Mutex<Option<mpsc::UnboundedSender<Command>>>,
    /// The auth the current socket task is bound to. A different auth (login
    /// change) resets the manager so the new credentials' sockets replace the old.
    auth_key: Mutex<Option<AuthKey>>,
}

/// Identifies the connection a socket task is bound to: either anonymous or a
/// specific logged-in user (keyed on login + token so a re-login rebinds).
#[derive(Clone, PartialEq)]
enum AuthKey {
    Anonymous,
    User { login: String, token: String },
}

impl AuthKey {
    fn from_auth(auth: &Option<TwitchAuth>) -> Self {
        match auth {
            Some(a) => AuthKey::User {
                login: a.login.clone(),
                token: a.oauth_pass.clone(),
            },
            None => AuthKey::Anonymous,
        }
    }
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

/// Registers `channel` on the app-wide shared IRC connection matching `auth`
/// (anonymous or the logged-in user), routing its messages into `sink`. Returns a
/// guard that keeps the channel joined until dropped.
///
/// Must run on the tokio runtime (it spawns the socket task on first use / rebind).
pub fn register(auth: Option<TwitchAuth>, channel: String, sink: ChatSink) -> Registration {
    let manager = manager();
    let key = AuthKey::from_auth(&auth);

    // Bind (or rebind on a login change) the socket task to this auth. A rebind
    // drops the old command sender, which ends the old task and closes its
    // sockets; a fresh one starts for the new credentials.
    {
        let mut cur = manager.auth_key.lock().unwrap();
        let mut tx_slot = manager.tx.lock().unwrap();
        if cur.as_ref() != Some(&key) || tx_slot.is_none() {
            let (tx, rx) = mpsc::unbounded_channel::<Command>();
            *tx_slot = Some(tx);
            *cur = Some(key);
            tokio::spawn(async move { socket_task(auth, rx).await });
        }
    }

    manager.send(Command::Register {
        channel: channel.clone(),
        sink,
    });

    Registration {
        channel,
        manager: manager.clone(),
    }
}

/// One channel's routing state on the shared connection.
struct Channel {
    sink: ChatSink,
    /// Whether we've emitted this channel's `ChannelMeta` yet (from ROOMSTATE or
    /// the first PRIVMSG) — so a reconnect's fresh ROOMSTATE doesn't re-emit it
    /// (which would re-fetch emotes/badges + re-run history).
    channel_meta_sent: bool,
    /// The channel's current chat-restriction modes, merged from (partial)
    /// ROOMSTATEs so each emitted `ChatEvent::ChatModes` is a full snapshot.
    modes: ChatModes,
    /// Whether this session has emitted a modes snapshot yet. The first
    /// ROOMSTATE of a session always emits (even if nothing differs from our
    /// merged state) so a mode toggled while we were disconnected can't leave
    /// the UI stale; after that only real changes emit.
    modes_synced: bool,
}

/// A leaky-bucket rate limiter for JOINs (budget tokens per cooldown
/// window). `take()` returns how long to wait before
/// this JOIN may go out; the caller sleeps that long (0 when a token is free).
struct JoinBucket {
    /// Instants at which past JOINs were sent, oldest first; entries older than
    /// one cooldown window are pruned on each `take`.
    sent: std::collections::VecDeque<std::time::Instant>,
}

impl JoinBucket {
    fn new() -> Self {
        Self {
            sent: std::collections::VecDeque::new(),
        }
    }

    /// Reserves a JOIN slot, returning the delay to wait before sending it.
    fn take(&mut self) -> std::time::Duration {
        let now = std::time::Instant::now();
        while let Some(&front) = self.sent.front() {
            if now.duration_since(front) >= JOIN_RATELIMIT_COOLDOWN {
                self.sent.pop_front();
            } else {
                break;
            }
        }
        let delay = if self.sent.len() >= JOIN_RATELIMIT_BUDGET {
            // Wait until the oldest in-window JOIN ages out of the window.
            let front = *self.sent.front().unwrap();
            JOIN_RATELIMIT_COOLDOWN.saturating_sub(now.duration_since(front))
        } else {
            std::time::Duration::ZERO
        };
        self.sent.push_back(now + delay);
        delay
    }
}

/// Drives the shared connection for one auth until the command sender is dropped
/// (login change / manager reset). Reconnects with backoff internally.
async fn socket_task(auth: Option<TwitchAuth>, mut commands: mpsc::UnboundedReceiver<Command>) {
    // The set of registered channels survives reconnects; only their live JOIN /
    // self-state is rebuilt each session.
    let mut channels: HashMap<String, Channel> = HashMap::new();
    let mut attempt: u32 = 0;

    loop {
        let started = std::time::Instant::now();
        let outcome = run_session(&auth, &mut channels, &mut commands).await;
        // A reconnect re-JOINs and re-observes ROOMSTATE, so reset the per-session
        // flag (but keep the channels + sinks).
        for ch in channels.values_mut() {
            ch.channel_meta_sent = false;
            ch.modes_synced = false;
        }
        match outcome {
            SessionOutcome::Shutdown => break, // command sender dropped — task retired
            SessionOutcome::Transient(err) => {
                // A session that held for a while is a fresh outage, not a
                // continuation of the previous backoff.
                if started.elapsed() > std::time::Duration::from_secs(60) {
                    attempt = 0;
                }
                let delay = bks_core::reconnect_delay(attempt);
                // First failure of an outage is user-visible on every channel; the
                // retries behind it are just logged so a flapping network doesn't
                // fill chat with error rows.
                if attempt == 0 {
                    for ch in channels.values() {
                        let _ = ch.sink.send(ChatEvent::Error(format!(
                            "twitch error: {err:#} — reconnecting in {}s",
                            delay.as_secs()
                        )));
                    }
                } else {
                    tracing::warn!("twitch reconnect attempt {attempt} failed: {err:#}");
                }
                if sleep_or_shutdown(delay, &mut commands).await {
                    break;
                }
                attempt += 1;
            }
        }
    }
}

enum SessionOutcome {
    /// The command channel closed — the task should retire.
    Shutdown,
    /// A transient failure (socket drop, connect error, reconnect request).
    Transient(anyhow::Error),
}

/// Runs one connection session: connect the read (and, when authed, write)
/// client, JOIN every registered channel, then pump incoming messages + commands
/// until a socket dies or the command channel closes.
async fn run_session(
    auth: &Option<TwitchAuth>,
    channels: &mut HashMap<String, Channel>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> SessionOutcome {
    use anyhow::Context;

    // The read client receives chat for every channel. When authenticated we open
    // a second, write client: sends go out on it, and it also
    // receives USERSTATE/NOTICE responses to our sends. Anonymous connections have
    // no write client (sending errors upstream before reaching here).
    let mut read = match connect(auth).await.context("connecting to Twitch (read)") {
        Ok(c) => c,
        Err(err) => return SessionOutcome::Transient(err),
    };
    let mut write = if auth.is_some() {
        match connect(auth).await.context("connecting to Twitch (write)") {
            Ok(c) => Some(c),
            Err(err) => return SessionOutcome::Transient(err),
        }
    } else {
        None
    };

    let mut join_bucket = JoinBucket::new();
    for channel in channels.keys().cloned().collect::<Vec<_>>() {
        if let Err(err) = join_channel(&mut read, &channel, &mut join_bucket).await {
            return SessionOutcome::Transient(err);
        }
    }

    loop {
        tokio::select! {
            biased;
            cmd = commands.recv() => match cmd {
                None => return SessionOutcome::Shutdown,
                Some(Command::Register { channel, sink }) => {
                    channels.insert(channel.clone(), Channel {
                        sink,
                        channel_meta_sent: false,
                        modes: ChatModes::default(),
                        modes_synced: false,
                    });
                    if let Err(err) = join_channel(&mut read, &channel, &mut join_bucket).await {
                        return SessionOutcome::Transient(err);
                    }
                }
                Some(Command::Unregister { channel }) => {
                    if channels.remove(&channel).is_some() {
                        let part = format!("PART {channel}\r\n");
                        let _ = read.send_raw(part.as_str()).await;
                    }
                }
                Some(Command::Send { channel, text, reply_parent_id }) => {
                    if let Some(w) = write.as_mut() {
                        send_message(
                            w, channels, &channel, &text,
                            reply_parent_id.as_deref(),
                        ).await;
                    }
                }
            },
            msg = read.recv() => {
                let msg = match msg.context("receiving message") {
                    Ok(m) => m,
                    Err(err) => return SessionOutcome::Transient(err),
                };
                if let Err(err) = handle_read(&mut read, &msg, channels).await {
                    return SessionOutcome::Transient(err);
                }
            }
            // Drain the write client's incoming stream (USERSTATE/NOTICE responses
            // to our sends). `recv()` never resolves when there's no write client.
            msg = recv_opt(write.as_mut()) => {
                let msg = match msg.context("receiving message (write)") {
                    Ok(m) => m,
                    Err(err) => return SessionOutcome::Transient(err),
                };
                if let Err(err) = handle_write(write.as_mut().unwrap(), &msg).await {
                    return SessionOutcome::Transient(err);
                }
            }
        }
    }
}

/// Connects a tmi client, authenticated when `auth` is present.
async fn connect(auth: &Option<TwitchAuth>) -> anyhow::Result<tmi::Client> {
    match auth {
        Some(a) => {
            let creds = tmi::Credentials::new(a.login.clone(), a.oauth_pass.clone());
            Ok(tmi::Client::builder().credentials(creds).connect().await?)
        }
        None => Ok(tmi::Client::anonymous().await?),
    }
}

/// JOINs `channel` on the read client, honoring the join rate-limit bucket.
async fn join_channel(
    read: &mut tmi::Client,
    channel: &str,
    bucket: &mut JoinBucket,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let delay = bucket.take();
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    read.join(channel)
        .await
        .with_context(|| format!("joining {channel}"))
}

/// Awaits the next message from an optional write client, never resolving when
/// there is none (so the `select!` branch is effectively disabled).
async fn recv_opt(
    write: Option<&mut tmi::Client>,
) -> Result<tmi::IrcMessage, tmi::client::read::RecvError> {
    match write {
        Some(c) => c.recv().await,
        None => std::future::pending().await,
    }
}

/// Handles one message off the read connection: route chat/notices to the owning
/// channel's sink, answer pings, and reconnect on a Twitch RECONNECT.
async fn handle_read(
    read: &mut tmi::Client,
    msg: &tmi::IrcMessage,
    channels: &mut HashMap<String, Channel>,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let typed = msg.as_typed().context("parsing message")?;
    match typed {
        tmi::Message::RoomState(rs) => {
            let key = channel_key(rs.channel());
            if let Some(ch) = channels.get_mut(&key) {
                if !ch.channel_meta_sent {
                    ch.channel_meta_sent = true;
                    let _ = ch.sink.send(ChatEvent::Channel(build_channel_meta(
                        rs.channel(),
                        rs.channel_id(),
                    )));
                }
                if merge_roomstate(&mut ch.modes, &rs) || !ch.modes_synced {
                    ch.modes_synced = true;
                    let _ = ch.sink.send(ChatEvent::ChatModes {
                        platform: Platform::Twitch,
                        modes: ch.modes,
                    });
                }
            }
        }
        tmi::Message::Privmsg(pm) => {
            let key = channel_key(pm.channel());
            if let Some(ch) = channels.get_mut(&key) {
                if !ch.channel_meta_sent {
                    ch.channel_meta_sent = true;
                    let _ = ch.sink.send(ChatEvent::Channel(build_channel_meta(
                        pm.channel(),
                        pm.channel_id(),
                    )));
                }
                let first_message = msg.tag(tmi::Tag::FirstMsg) == Some("1");
                let message = privmsg_to_message(pm.channel(), &pm, first_message);
                let _ = ch.sink.send(ChatEvent::Message(Box::new(message)));
            }
        }
        tmi::Message::UserState(us) => {
            if let Some(ch) = channels.get(&channel_key(us.channel())) {
                let state = SelfState::from_userstate(&us);
                let _ = ch.sink.send(ChatEvent::ModStatus {
                    platform: Platform::Twitch,
                    is_mod: state.is_moderator(),
                    is_broadcaster: state.is_broadcaster(),
                });
            }
        }
        tmi::Message::ClearChat(cc) => {
            if let Some(ch) = channels.get(&channel_key(cc.channel())) {
                let _ = ch.sink.send(clearchat_event(&cc, false));
            }
        }
        tmi::Message::ClearMsg(cm) => {
            if let Some(ch) = channels.get(&channel_key(cm.channel())) {
                let _ = ch.sink.send(ChatEvent::DeleteMessage {
                    platform: Platform::Twitch,
                    message_id: cm.target_message_id().to_string(),
                });
            }
        }
        tmi::Message::UserNotice(un) => {
            if let Some(ch) = channels.get(&channel_key(un.channel())) {
                // `msg-param-value` (the authoritative watch-streak length) is
                // dropped by tmi's parse, so read it off the raw message here.
                let milestone_value = msg
                    .tag(tmi::Tag::from("msg-param-value"))
                    .and_then(|v| v.parse().ok());
                if let Some(event) = usernotice_event(&un, milestone_value) {
                    let _ = ch.sink.send(event);
                }
            }
        }
        // Twitch asked us to reconnect: reconnect this same read client in place
        // and re-JOIN every channel on it (a fresh session drops the joins).
        tmi::Message::Reconnect => {
            read.reconnect().await.context("reconnecting")?;
            let mut bucket = JoinBucket::new();
            for channel in channels.keys().cloned().collect::<Vec<_>>() {
                join_channel(read, &channel, &mut bucket).await?;
            }
        }
        tmi::Message::Ping(ping) => {
            read.pong(&ping).await.context("ponging")?;
        }
        _ => {}
    }
    Ok(())
}

/// Handles one message off the write connection. We only keep it drained
/// (answering pings, honoring a RECONNECT) so its socket buffer never stalls; the
/// write connection carries no display data (chat arrives on the read connection).
async fn handle_write(write: &mut tmi::Client, msg: &tmi::IrcMessage) -> anyhow::Result<()> {
    use anyhow::Context;
    match msg.as_typed().context("parsing message (write)")? {
        tmi::Message::Reconnect => {
            write.reconnect().await.context("reconnecting (write)")?;
        }
        tmi::Message::Ping(ping) => {
            write.pong(&ping).await.context("ponging (write)")?;
        }
        _ => {}
    }
    Ok(())
}

/// Sends a message (or reply) on the write client. We do NOT synthesize a local
/// echo: the shared **read** connection is a separate session logged in as the
/// same user, so Twitch broadcasts our sent message back to it as a normal
/// PRIVMSG (which then renders with our real badges/color/id). A local echo would
/// double every message we send — only the read connection joins channels, and
/// it receives the write connection's messages.
async fn send_message(
    write: &mut tmi::Client,
    channels: &HashMap<String, Channel>,
    channel: &str,
    text: &str,
    reply_parent_id: Option<&str>,
) {
    let Some(ch) = channels.get(&channel_key(channel)) else {
        return;
    };
    let mut pm = write.privmsg(channel, text);
    if let Some(id) = reply_parent_id {
        pm = pm.reply_to(id);
    }
    if let Err(err) = pm.send().await {
        // The user's message didn't go out — show it, don't log it.
        let _ = ch
            .sink
            .send(ChatEvent::Error(format!("twitch send failed: {err}")));
    }
}

/// Merges a ROOMSTATE into the channel's current modes, returning whether
/// anything changed. Twitch's ROOMSTATE is a *delta*: the one sent on JOIN
/// carries every tag, later ones only the changed tag (tmi maps a missing tag
/// to `None` = no change). `followers-only` folds its enabled-with-no-minimum
/// case into a zero duration; a zero `slow` means slow mode is off.
fn merge_roomstate(modes: &mut ChatModes, rs: &tmi::RoomState) -> bool {
    let before = *modes;
    if let Some(on) = rs.emote_only() {
        modes.emote_only = on;
    }
    if let Some(fo) = rs.followers_only() {
        modes.followers_only = match fo {
            tmi::FollowersOnly::Disabled => None,
            tmi::FollowersOnly::Enabled(min) => Some(min.unwrap_or_default()),
        };
    }
    if let Some(on) = rs.r9k() {
        modes.unique = on;
    }
    if let Some(d) = rs.slow() {
        modes.slow = (!d.is_zero()).then_some(d);
    }
    if let Some(on) = rs.subs_only() {
        modes.subscribers_only = on;
    }
    before != *modes
}

/// The routing key for a channel: `#name` lowercased, matching how tabs register.
fn channel_key(channel: &str) -> String {
    format!("#{}", bks_core::channel_login(channel))
}

/// Sleeps for `dur` but wakes early (returning `true`) if the command channel
/// closes meanwhile, so a retired task doesn't dawdle in a backoff sleep.
async fn sleep_or_shutdown(
    dur: std::time::Duration,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        cmd = commands.recv() => cmd.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tmi::{FromIrc, IrcMessageRef};

    fn roomstate(tags: &str) -> tmi::RoomState<'static> {
        let raw = format!("@room-id=12345;{tags} :tmi.twitch.tv ROOMSTATE #oilrats");
        let irc = IrcMessageRef::parse(&raw).unwrap();
        tmi::RoomState::from_irc(irc).unwrap().into_owned()
    }

    #[test]
    fn full_roomstate_sets_every_mode() {
        let mut modes = ChatModes::default();
        let changed = merge_roomstate(
            &mut modes,
            &roomstate("emote-only=1;followers-only=10;r9k=1;slow=5;subs-only=1"),
        );
        assert!(changed);
        assert_eq!(
            modes,
            ChatModes {
                emote_only: true,
                subscribers_only: true,
                followers_only: Some(Duration::from_secs(600)),
                slow: Some(Duration::from_secs(5)),
                unique: true,
            }
        );
    }

    #[test]
    fn delta_roomstate_touches_only_its_tag() {
        let mut modes = ChatModes {
            slow: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        // Emote-only flips on; slow (absent = no change) must survive.
        assert!(merge_roomstate(&mut modes, &roomstate("emote-only=1")));
        assert!(modes.emote_only);
        assert_eq!(modes.slow, Some(Duration::from_secs(5)));
        // slow=0 turns slow mode off.
        assert!(merge_roomstate(&mut modes, &roomstate("slow=0")));
        assert_eq!(modes.slow, None);
        // followers-only=-1 is off, =0 is "any follower" (zero minimum).
        assert!(merge_roomstate(&mut modes, &roomstate("followers-only=0")));
        assert_eq!(modes.followers_only, Some(Duration::ZERO));
        assert!(merge_roomstate(&mut modes, &roomstate("followers-only=-1")));
        assert_eq!(modes.followers_only, None);
    }

    #[test]
    fn unchanged_roomstate_reports_no_change() {
        let mut modes = ChatModes::default();
        assert!(!merge_roomstate(
            &mut modes,
            &roomstate("emote-only=0;followers-only=-1;r9k=0;slow=0;subs-only=0"),
        ));
        assert!(!modes.any());
    }
}
