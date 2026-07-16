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
//! routes each incoming message to the registered sinks by `#channel`. A channel
//! can hold **several sinks at once** (two channel models sharing one Twitch
//! channel under different channel-set keys, or an old connection overlapping
//! its replacement during a tab edit) — each registration has a unique id, so a
//! guard's drop removes exactly its own sink, and the channel is only PARTed
//! when its last sink unregisters. Sends and replies for a channel are handed to
//! the write client, which also echoes our own message back (Twitch doesn't).
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
    /// Add a registration: JOIN the channel (on the live session, if any, unless
    /// already joined) and route its messages into `sink` alongside any other
    /// sinks the channel has.
    Register {
        channel: String,
        id: u64,
        sink: ChatSink,
    },
    /// Remove one registration by id; PART the channel once no sinks remain.
    Unregister { channel: String, id: u64 },
    /// Send a message (or reply) to a channel via the write client. The read
    /// connection delivers it back as a real PRIVMSG, so no local echo is made.
    Send {
        channel: String,
        text: String,
        reply_parent_id: Option<String>,
    },
}

/// A live channel registration on the shared IRC connection. The channel stays
/// joined as long as this guard lives; dropping it removes this registration's
/// sink (identified by `id` — never another registration of the same channel)
/// and PARTs the channel once no sinks remain.
pub struct Registration {
    channel: String,
    id: u64,
    manager: Arc<ManagerInner>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.manager.send(Command::Unregister {
            channel: std::mem::take(&mut self.channel),
            id: self.id,
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

    // Process-unique, so an old guard's deferred drop (its connection winding
    // down after a rebind or tab edit) can never unregister a successor's
    // registration of the same channel.
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    manager.send(Command::Register {
        channel: channel.clone(),
        id,
        sink,
    });

    Registration {
        channel,
        id,
        manager: manager.clone(),
    }
}

/// One registration's routing state: its sink plus the per-connection flags
/// that used to live on the channel (each sink is one channel model's stream,
/// so each needs its own `ChannelMeta`/modes handshake).
struct SinkState {
    sink: ChatSink,
    /// Whether this sink has received the channel's `ChannelMeta` yet (from
    /// ROOMSTATE, the first PRIVMSG, or the cached id on a late attach) — so a
    /// reconnect's fresh ROOMSTATE doesn't re-emit it (which would re-fetch
    /// emotes/badges + re-run history).
    channel_meta_sent: bool,
    /// Whether this sink has received a modes snapshot this session. The first
    /// ROOMSTATE of a session always emits (even if nothing differs from our
    /// merged state) so a mode toggled while we were disconnected can't leave
    /// the UI stale; after that only real changes emit.
    modes_synced: bool,
}

impl SinkState {
    fn new(sink: ChatSink) -> Self {
        Self {
            sink,
            channel_meta_sent: false,
            modes_synced: false,
        }
    }
}

/// One channel's routing state on the shared connection: every registered sink
/// (keyed by registration id) plus the channel-level facts they share.
struct Channel {
    sinks: HashMap<u64, SinkState>,
    /// The channel's numeric room id, learned from the first ROOMSTATE/PRIVMSG
    /// and kept across reconnects (it's stable). Lets a sink that registers onto
    /// an *already joined* channel get its `ChannelMeta` immediately — no fresh
    /// ROOMSTATE arrives without a re-JOIN.
    channel_id: Option<String>,
    /// The channel's current chat-restriction modes, merged from (partial)
    /// ROOMSTATEs so each emitted `ChatEvent::ChatModes` is a full snapshot.
    modes: ChatModes,
}

impl Channel {
    fn new(id: u64, sink: ChatSink) -> Self {
        Self {
            sinks: HashMap::from([(id, SinkState::new(sink))]),
            channel_id: None,
            modes: ChatModes::default(),
        }
    }
}

/// Adds a registration's sink to the channel map, returning whether this
/// *created* the channel entry (i.e. a live session must JOIN it). Shared by the
/// in-session Register arm and the backoff-sleep command drain.
fn add_sink(
    channels: &mut HashMap<String, Channel>,
    channel: String,
    id: u64,
    sink: ChatSink,
) -> bool {
    use std::collections::hash_map::Entry;
    match channels.entry(channel) {
        Entry::Occupied(mut e) => {
            e.get_mut().sinks.insert(id, SinkState::new(sink));
            false
        }
        Entry::Vacant(e) => {
            e.insert(Channel::new(id, sink));
            true
        }
    }
}

/// Removes a registration's sink by id, returning whether the channel entry is
/// now gone (i.e. a live session should PART it). An unknown channel/id (a
/// stale guard from a retired connection) is a no-op.
fn remove_sink(channels: &mut HashMap<String, Channel>, channel: &str, id: u64) -> bool {
    let Some(ch) = channels.get_mut(channel) else {
        return false;
    };
    ch.sinks.remove(&id);
    if ch.sinks.is_empty() {
        channels.remove(channel);
        true
    } else {
        false
    }
}

/// Sends the channel's `ChannelMeta` to every sink that hasn't had one yet
/// (no-op until the channel id is known).
fn send_pending_meta(ch: &mut Channel, channel: &str) {
    let Some(id) = &ch.channel_id else { return };
    for s in ch.sinks.values_mut() {
        if !s.channel_meta_sent {
            s.channel_meta_sent = true;
            let _ = s
                .sink
                .send(ChatEvent::Channel(build_channel_meta(channel, id)));
        }
    }
}

/// Fans one event out to every sink of a channel; the last send moves the event
/// so the (common) single-sink case clones nothing.
fn fan_out(ch: &Channel, event: ChatEvent) {
    let mut event = Some(event);
    let mut sinks = ch.sinks.values().peekable();
    while let Some(s) = sinks.next() {
        let payload = if sinks.peek().is_some() {
            event.clone().expect("payload present until the last sink")
        } else {
            event.take().expect("payload present for the last sink")
        };
        let _ = s.sink.send(payload);
    }
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
        // flags (but keep the channels + sinks + cached channel ids).
        for ch in channels.values_mut() {
            for s in ch.sinks.values_mut() {
                s.channel_meta_sent = false;
                s.modes_synced = false;
            }
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
                        for s in ch.sinks.values() {
                            let _ = s.sink.send(ChatEvent::Error(format!(
                                "twitch error: {err:#} — reconnecting in {}s",
                                delay.as_secs()
                            )));
                        }
                    }
                } else {
                    tracing::warn!("twitch reconnect attempt {attempt} failed: {err:#}");
                }
                if sleep_or_shutdown(delay, &mut channels, &mut commands).await {
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
                Some(Command::Register { channel, id, sink }) => {
                    if add_sink(channels, channel.clone(), id, sink) {
                        if let Err(err) = join_channel(&mut read, &channel, &mut join_bucket).await {
                            return SessionOutcome::Transient(err);
                        }
                    } else if let Some(ch) = channels.get_mut(&channel) {
                        // The channel is already joined, so no fresh ROOMSTATE
                        // will arrive for this sink — hand it the cached
                        // identity + modes snapshot directly (the id is stable
                        // across sessions; on the rare attach before the first
                        // ROOMSTATE it just waits for that like the first sink).
                        if ch.channel_id.is_some() {
                            send_pending_meta(ch, &channel);
                            if let Some(s) = ch.sinks.get_mut(&id) {
                                s.modes_synced = true;
                                let _ = s.sink.send(ChatEvent::ChatModes {
                                    platform: Platform::Twitch,
                                    modes: ch.modes,
                                });
                            }
                        }
                    }
                }
                Some(Command::Unregister { channel, id }) => {
                    if remove_sink(channels, &channel, id) {
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
                ch.channel_id = Some(rs.channel_id().to_string());
                send_pending_meta(ch, rs.channel());
                let changed = merge_roomstate(&mut ch.modes, &rs);
                for s in ch.sinks.values_mut() {
                    if changed || !s.modes_synced {
                        s.modes_synced = true;
                        let _ = s.sink.send(ChatEvent::ChatModes {
                            platform: Platform::Twitch,
                            modes: ch.modes,
                        });
                    }
                }
            }
        }
        tmi::Message::Privmsg(pm) => {
            let key = channel_key(pm.channel());
            if let Some(ch) = channels.get_mut(&key) {
                ch.channel_id = Some(pm.channel_id().to_string());
                send_pending_meta(ch, pm.channel());
                let first_message = msg.tag(tmi::Tag::FirstMsg) == Some("1");
                let message = privmsg_to_message(pm.channel(), &pm, first_message);
                fan_out(ch, ChatEvent::Message(Box::new(message)));
            }
        }
        tmi::Message::UserState(us) => {
            if let Some(ch) = channels.get(&channel_key(us.channel())) {
                let state = SelfState::from_userstate(&us);
                fan_out(
                    ch,
                    ChatEvent::ModStatus {
                        platform: Platform::Twitch,
                        is_mod: state.is_moderator(),
                        is_broadcaster: state.is_broadcaster(),
                    },
                );
            }
        }
        tmi::Message::ClearChat(cc) => {
            if let Some(ch) = channels.get(&channel_key(cc.channel())) {
                fan_out(ch, clearchat_event(&cc, false));
            }
        }
        tmi::Message::ClearMsg(cm) => {
            if let Some(ch) = channels.get(&channel_key(cm.channel())) {
                fan_out(
                    ch,
                    ChatEvent::DeleteMessage {
                        platform: Platform::Twitch,
                        message_id: cm.target_message_id().to_string(),
                    },
                );
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
                    fan_out(ch, event);
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
        fan_out(ch, ChatEvent::Error(format!("twitch send failed: {err}")));
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
/// Commands arriving during the sleep are **applied to the channel map**, not
/// discarded (the old version dropped them: a tab registered during a backoff
/// was never joined, and a dropped Unregister leaked its sink + JOIN forever) —
/// the upcoming session JOINs everything registered, so no join happens here.
/// A `Send` has no connection to go out on; its channel's sinks get the error.
async fn sleep_or_shutdown(
    dur: std::time::Duration,
    channels: &mut HashMap<String, Channel>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
) -> bool {
    let deadline = tokio::time::Instant::now() + dur;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            cmd = commands.recv() => match cmd {
                None => return true,
                Some(Command::Register { channel, id, sink }) => {
                    add_sink(channels, channel, id, sink);
                }
                Some(Command::Unregister { channel, id }) => {
                    remove_sink(channels, &channel, id);
                }
                Some(Command::Send { channel, .. }) => {
                    if let Some(ch) = channels.get(&channel_key(&channel)) {
                        fan_out(
                            ch,
                            ChatEvent::Error(
                                "twitch send failed: not connected (reconnecting)".to_string(),
                            ),
                        );
                    }
                }
            },
        }
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

    #[test]
    fn same_channel_registrations_coexist_and_unregister_independently() {
        // Two channel models can share one Twitch channel under different
        // channel-set keys (or overlap during a tab edit); the second register
        // must not clobber the first's sink, and an unregister must remove only
        // its own registration — the old channel-keyed map killed the survivor.
        let mut channels: HashMap<String, Channel> = HashMap::new();
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        assert!(add_sink(&mut channels, "#chan".into(), 1, tx_a));
        assert!(!add_sink(&mut channels, "#chan".into(), 2, tx_b));

        // Both sinks receive fan-outs.
        let ch = channels.get("#chan").unwrap();
        fan_out(ch, ChatEvent::Notice("hi".into()));
        assert!(matches!(rx_a.try_recv(), Ok(ChatEvent::Notice(_))));
        assert!(matches!(rx_b.try_recv(), Ok(ChatEvent::Notice(_))));

        // A stale guard's unregister (unknown id) is a no-op; removing one
        // registration keeps the channel; removing the last one drops it.
        assert!(!remove_sink(&mut channels, "#chan", 99));
        assert!(!remove_sink(&mut channels, "#chan", 1));
        assert!(channels.contains_key("#chan"));
        assert!(remove_sink(&mut channels, "#chan", 2));
        assert!(!channels.contains_key("#chan"));
    }

    #[test]
    fn pending_meta_goes_only_to_sinks_that_lack_it() {
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let mut ch = Channel::new(1, tx_a);
        ch.sinks.insert(2, SinkState::new(tx_b));

        // No channel id yet → nothing to send.
        send_pending_meta(&mut ch, "#chan");
        assert!(rx_a.try_recv().is_err());

        ch.channel_id = Some("12345".to_string());
        ch.sinks.get_mut(&1).unwrap().channel_meta_sent = true;
        send_pending_meta(&mut ch, "#chan");
        // Sink 1 already had its meta; only sink 2 gets one (exactly once).
        assert!(rx_a.try_recv().is_err());
        match rx_b.try_recv() {
            Ok(ChatEvent::Channel(meta)) => {
                assert_eq!(meta.id, "12345");
                assert_eq!(meta.name, "chan");
            }
            other => panic!("expected ChannelMeta, got {other:?}"),
        }
        send_pending_meta(&mut ch, "#chan");
        assert!(rx_b.try_recv().is_err());
    }
}
