//! Bridges the tokio world (where the connectors live) to GPUI's executor.
//!
//! A single multi-threaded tokio runtime is kept alive for the program's
//! lifetime. Each connector drains its tokio stream and forwards events onto a
//! shared `smol` channel, which GPUI can await directly inside `cx.spawn`. The
//! tokio mpsc type never crosses into the UI.
//!
//! Twitch and Kick run as independent tasks feeding the *same* channel, so the
//! UI shows one merged feed (each message tagged by its platform). Adding
//! another platform = spawn one more task here.
//!
//! This is also where 3rd-party emotes are resolved: on the `Channel` event we
//! load every configured [`EmoteProvider`], then rewrite each later message's
//! text runs. Twitch gets 7TV + BTTV + FFZ; Kick gets 7TV (and also sends its
//! native emotes inline, already parsed before they reach here). Adding another
//! provider is pushing it into `providers()`/`kick_providers()`.

use std::sync::{Arc, OnceLock};

use bks_emotes::{BttvProvider, EmoteProvider, EmoteRegistry, FfzProvider, SeventvProvider};
use bks_kick::KickSource;
use bks_platform::{ChannelMeta, ChatEvent, ChatSource, LastStream};
use bks_twitch::{BadgeMap, EventsubAuth, TwitchSource};
use bks_youtube::YouTubeSource;
use tokio::runtime::Runtime;

use crate::controller::Controller;
use crate::session::Session;

/// A clonable sender into the merged UI event stream.
type Sink = smol::channel::Sender<ChatEvent>;

/// The shared multi-threaded tokio runtime (one for the whole app).
pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime")
    })
}

/// Connects one tab to its configured channels and returns the tab's chat-event
/// receiver plus a [`Controller`] for sending/moderating. Either channel may be
/// empty (the empty platform is skipped). Login is shared via `session`.
pub fn connect(
    session: Session,
    twitch_channel: &str,
    kick_channel: &str,
    youtube_channel: &str,
) -> (smol::channel::Receiver<ChatEvent>, Controller) {
    let (tx, rx) = smol::channel::unbounded::<ChatEvent>();
    let twitch = bks_core::strip_channel(twitch_channel).to_string();
    let kick = bks_core::strip_channel(kick_channel).to_string();
    // YouTube's source is a handle/URL/video ref, not a `#channel`, so it's passed
    // through verbatim (just trimmed).
    let youtube = youtube_channel.trim().to_string();

    let controller = Controller::new(
        session,
        tx.clone(),
        runtime().handle().clone(),
        twitch.clone(),
        kick.clone(),
    );

    // Twitch: the controller owns the connection (authed if logged in, else
    // anonymous) so login swaps can re-join cleanly. Skipped if no channel.
    controller.start();

    // Twitch live status is polled (via IVR, anonymous): an immediate first check,
    // then every LIVE_POLL_SECS, emitting a `Live` event on a transition. Kick does
    // NOT poll its live *status* — its `StreamerIsLive`/`StopStreamBroadcast` arrive
    // on the Pusher `channel.{id}` subscription the connector already holds
    // (real-time, no poll) — but its viewer *count* has no push event, so that one
    // number is polled from the lightweight livestream endpoint.
    if !twitch.is_empty() {
        runtime().spawn(poll_twitch_live(twitch.clone(), tx.clone()));
    }

    // Kick: emotes arrive inline, so messages forward unchanged. We also record
    // each chatter's id with the controller so Kick moderation can target them.
    if !kick.is_empty() {
        runtime().spawn(poll_kick_viewers(kick.clone(), tx.clone()));
        runtime().spawn(run_kick(
            Arc::new(KickSource::new()),
            kick,
            tx.clone(),
            controller.clone(),
        ));
    }

    // YouTube: anonymous InnerTube read. Like Kick, emotes arrive inline (custom
    // channel emojis), so on top of that we only add 7TV. No moderation yet.
    if !youtube.is_empty() {
        runtime().spawn(run_youtube(Arc::new(YouTubeSource::new()), youtube, tx));
    }

    (rx, controller)
}

/// How often to re-check a channel's live status (Chatterino's 30s).
const LIVE_POLL_SECS: u64 = 30;
/// The Kick viewer-count poll's slower cadence while the channel is offline —
/// going live is push-based (Pusher), so only the first *count* waits this long.
const KICK_OFFLINE_POLL_SECS: u64 = 120;

/// One live-status observation, compared across polls so a `Live` event is only
/// emitted on a change: (live, title, game, started_at, last_stream).
type LiveSnapshot = (
    bool,
    String,
    String,
    Option<chrono::DateTime<chrono::Utc>>,
    Option<LastStream>,
);

/// Polls Twitch live status (via IVR, anonymous) and emits a `Live` event on
/// every transition — including the first observation if the channel is live, so
/// opening a tab on an already-live stream still shows the notice.
async fn poll_twitch_live(channel: String, tx: Sink) {
    // The last-broadcast lookup (a second request) runs once per offline
    // stretch, not on every 30s poll: the cache holds the fetched result
    // (`Some(None)` = fetched, channel has no VODs) and is cleared while live so
    // the fresh VOD is picked up after the stream ends.
    let last_cache: Arc<tokio::sync::Mutex<Option<Option<LastStream>>>> = Arc::default();
    // The viewer count is pushed by Hermes, but only every ~30s — seed it once
    // per live stretch from GQL so the bar doesn't sit at a bare "LIVE" until
    // the first push (which then takes over).
    let seeded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seed_tx = tx.clone();
    poll_live(tx, bks_core::Platform::Twitch, move || {
        let channel = channel.clone();
        let last_cache = last_cache.clone();
        let seeded = seeded.clone();
        let seed_tx = seed_tx.clone();
        async move {
            let s = bks_twitch::fetch_live_status(&channel).await?;
            use std::sync::atomic::Ordering;
            if s.live {
                // Only a real number both marks the seed done and is emitted:
                // GQL lags IVR at go-live (and a hidden count is None forever),
                // and forwarding that None would delete a count Hermes already
                // pushed. A miss just retries on the next poll.
                if !seeded.load(Ordering::Relaxed) {
                    match bks_twitch::fetch_viewer_count(&channel).await {
                        Ok(Some(count)) => {
                            seeded.store(true, Ordering::Relaxed);
                            let _ = seed_tx
                                .send(ChatEvent::Viewers {
                                    platform: bks_core::Platform::Twitch,
                                    count: Some(count),
                                })
                                .await;
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::debug!("twitch viewer-count seed failed for {channel}: {err:#}");
                        }
                    }
                }
            } else {
                // Re-seed the next live stretch.
                seeded.store(false, Ordering::Relaxed);
            }
            let last_stream = if s.live {
                *last_cache.lock().await = None;
                None
            } else {
                let mut cache = last_cache.lock().await;
                if cache.is_none() {
                    // GQL's newest archive VOD has the full picture (start,
                    // length, category); IVR's `lastBroadcast` (start + title
                    // only) is the fallback for channels with VODs disabled.
                    let fetched = bks_twitch::fetch_last_stream(&channel)
                        .await
                        .unwrap_or_default()
                        .or_else(|| {
                            s.last_started_at.map(|started_at| LastStream {
                                started_at,
                                ended_at: None,
                                title: s.last_title.clone(),
                                game: String::new(),
                            })
                        });
                    *cache = Some(fetched);
                }
                cache.clone().flatten()
            };
            Ok((s.live, s.title, s.game, s.started_at, last_stream))
        }
    })
    .await;
}

/// Generic live-status poll loop (used by Twitch; Kick is push-based via Pusher).
/// Calls `check` every [`LIVE_POLL_SECS`] (with an immediate first call) and
/// forwards a `Live` event whenever the live flag changes from the previous
/// observation. A failed check is logged and skipped (the previous state is kept),
/// so a transient network blip doesn't spuriously toggle the status. Ends when the
/// UI drops the receiver. (Twitch's viewer *count* doesn't ride this poll: it's
/// pushed by the Hermes `video-playback-by-id` topic — see `twitch::pubsub` —
/// which is fresher than anything IVR/GQL serve.)
async fn poll_live<F, Fut>(tx: Sink, platform: bks_core::Platform, check: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<LiveSnapshot>>,
{
    // Track the whole snapshot, not just the live flag: re-emit when the title,
    // game, or start time changes too (e.g. the start time arrives a poll late, or
    // the stream's title/category/start updates), so the tab tooltip stays current
    // without needing a live↔offline flip. The first poll always emits (prev is
    // `None`).
    let mut prev: Option<LiveSnapshot> = None;
    loop {
        // The send below only fires on a *change*, so without this check a closed
        // tab would keep polling a stable channel every LIVE_POLL_SECS forever.
        if tx.is_closed() {
            break;
        }
        match check().await {
            Ok(snapshot) => {
                let (live, title, game, started_at, last_stream) = snapshot.clone();
                if prev.as_ref() != Some(&snapshot) {
                    prev = Some(snapshot);
                    let event = ChatEvent::Live {
                        platform,
                        live,
                        title,
                        game,
                        started_at,
                        last_stream,
                        link: None,
                    };
                    if tx.send(event).await.is_err() {
                        break; // UI side dropped the receiver.
                    }
                }
            }
            Err(err) => {
                tracing::warn!("live-status check failed for {}: {err:#}", platform.label());
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(LIVE_POLL_SECS)).await;
    }
}

/// Polls Kick's concurrent viewer count every [`LIVE_POLL_SECS`], emitting a
/// `Viewers` event on a change. Only the *count* is polled — Kick's live/offline
/// transitions stay push-based (Pusher) — because Kick has no viewer-count push
/// event. `Ok(None)` (offline) clears the count; a failed request keeps the
/// previous one.
async fn poll_kick_viewers(channel: String, tx: Sink) {
    let mut prev: Option<Option<u64>> = None;
    loop {
        if tx.is_closed() {
            break;
        }
        // Back off while offline: most Kick tabs are offline channels most of
        // the day, and the live *transition* arrives instantly via Pusher —
        // only the count (which the transition can't carry) waits for the next
        // slower tick. A failed request keeps the previous state and cadence.
        let mut offline = matches!(prev, Some(None));
        match bks_kick::fetch_viewer_count(&channel).await {
            Ok(count) => {
                offline = count.is_none();
                if prev != Some(count) {
                    prev = Some(count);
                    let event = ChatEvent::Viewers {
                        platform: bks_core::Platform::Kick,
                        count,
                    };
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
            Err(err) => {
                tracing::warn!("kick viewer-count check failed for {channel}: {err:#}");
            }
        }
        let secs = if offline {
            KICK_OFFLINE_POLL_SECS
        } else {
            LIVE_POLL_SECS
        };
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    }
}

/// Aborts its tasks on drop. Ties side feeds (the Hermes channel-point socket,
/// the EventSub moderator feed) to the IRC connection that spawned them: when
/// `run_twitch` returns (login swap, channel change, tab close), the guard drops
/// and the feeds' sockets close instead of lingering — a lingering feed plus the
/// replacement connection's fresh one would double every event. It also holds the
/// tab's shared-EventSub registration, so dropping the guard unregisters the
/// channel from the app-wide socket (freeing its subscription slots).
#[derive(Default)]
struct TaskGuard {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    eventsub: Option<bks_twitch::EventsubRegistration>,
}

impl TaskGuard {
    fn add(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.tasks.push(handle);
    }

    fn set_eventsub(&mut self, reg: Option<bks_twitch::EventsubRegistration>) {
        self.eventsub = reg;
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        for handle in &self.tasks {
            handle.abort();
        }
        // Dropping `eventsub` (a Registration) sends the unregister command.
    }
}

/// Drives a Twitch connection (anonymous or authenticated), loading + applying
/// 3rd-party emotes and badges. Reused for the post-login re-join so authed
/// messages get the same processing. `eventsub` (present when logged in) powers
/// the moderator feed — rich moderation notices + AutoMod — spawned alongside
/// the channel-point feed once the channel id is known.
pub async fn run_twitch(
    source: Arc<TwitchSource>,
    channel: String,
    tx: Sink,
    eventsub: Option<EventsubAuth>,
) {
    let emote_providers = providers();
    // Globals are shared app-wide (loaded once); this registry only adds the
    // channel's own emotes on top.
    let mut registry = EmoteRegistry::with_globals(twitch_globals().await);
    let mut badges = BadgeMap::default();
    // Chatters whose 7TV cosmetics we've already kicked off a lookup for, so each
    // user is resolved once per connection (the result is also process-cached).
    let mut seen_cosmetics: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut stream = match source.join(&channel).await {
        Ok(stream) => stream,
        Err(err) => {
            let _ = tx
                .send(ChatEvent::Error(format!("twitch failed: {err:#}")))
                .await;
            return;
        }
    };

    // Reset mod status for this fresh connection; an anonymous (logged-out)
    // join sends no USERSTATE, so without this a stale "mod" flag could linger
    // across a logout. An authed join's USERSTATE re-asserts it immediately.
    // Same for the moderator-feed flag: the previous connection's EventSub task
    // is aborted (no chance to send its own `active: false`), so a fresh
    // connection starts from "off" and this connection's feed re-asserts it.
    let _ = tx
        .send(ChatEvent::ModStatus {
            platform: bks_core::Platform::Twitch,
            is_mod: false,
            is_broadcaster: false,
        })
        .await;
    let _ = tx
        .send(ChatEvent::ModFeed {
            platform: bks_core::Platform::Twitch,
            active: false,
        })
        .await;

    // Channel-point redemptions ride a separate WebSocket (Hermes), not IRC —
    // and the EventSub moderator feed a third. Both are spawned once, when the
    // IRC connection hands over the channel id (via the `Channel` event) — they
    // need the numeric id to subscribe. The connector reconnects internally, so
    // `Channel` can arrive again; history is also fetched only on the first one
    // (a re-emit would duplicate the backlog). The guard aborts the side feeds
    // when this connection ends, so a re-join doesn't stack duplicates.
    let mut side_feeds_started = false;
    let mut side_feeds = TaskGuard::default();
    let mut history_done = false;

    while let Some(event) = stream.recv().await {
        let forward = match event {
            // Channel identity isn't shown in chat; use it to load 3rd-party
            // emotes + badge images, start the channel-point feed, then carry on
            // (the load status is logged, not shown in chat).
            ChatEvent::Channel(meta) => {
                if !side_feeds_started {
                    side_feeds_started = true;
                    spawn_pubsub(meta.id.clone(), tx.clone(), &mut side_feeds);
                    // The feed itself checks the token's scopes (logging the
                    // "/login again" hint when they're missing) and whether the
                    // user actually moderates this channel.
                    if let Some(auth) = eventsub.clone() {
                        spawn_eventsub(auth, meta.id.clone(), tx.clone(), &mut side_feeds);
                    }
                    // Seed the pinned banner with the channel's current pin. The
                    // Helix lookup is moderator-gated, so it only runs when logged
                    // in; a 403 (not a mod there) just means no seed — pins that
                    // happen while connected still arrive anonymously via Hermes.
                    if let Some(auth) = eventsub.clone() {
                        let (tx, channel_id) = (tx.clone(), meta.id.clone());
                        runtime().spawn(async move {
                            match bks_twitch::fetch_pinned_message(&auth, &channel_id).await {
                                Ok(Some((message, pinned_by, ends_at))) => {
                                    let _ = tx
                                        .send(ChatEvent::PinMessage {
                                            platform: bks_core::Platform::Twitch,
                                            pinned_by,
                                            message: Box::new(message),
                                            ends_at,
                                        })
                                        .await;
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    tracing::debug!("twitch pinned-message seed skipped: {err:#}")
                                }
                            }
                        });
                    }
                }
                // Emotes, badges, and the raw history backlog are three independent
                // network fetches; run them concurrently so the slowest one (not
                // their sum) gates the backlog. History is resolved *after* — it
                // needs the emote/badge results — but fetching it in parallel is
                // what makes Twitch history appear about as fast as Kick's (which
                // loads off its own task). The 7TV cache makes emotes near-instant
                // on a warm start, so a single history round-trip dominates.
                let raw_history;
                {
                    let badge_fut = load_badges(&meta.name);
                    let history_fut = async {
                        if history_done {
                            Ok(Vec::new()) // reconnect: backlog already shown
                        } else {
                            bks_twitch::fetch_recent(&meta.name, HISTORY_LIMIT).await
                        }
                    };
                    let emote_fut = load_emotes(&mut registry, &emote_providers, &meta);
                    let (_, b, h) = tokio::join!(emote_fut, badge_fut, history_fut);
                    badges = b;
                    raw_history = h;
                }
                // Hand the loaded emotes to the UI for the picker's Twitch tab
                // (text-only — they aren't shown in chat).
                let _ = tx
                    .send(ChatEvent::Emotes {
                        platform: bks_core::Platform::Twitch,
                        emotes: owned_emotes(registry.emotes(&meta.name)),
                    })
                    .await;
                // Resolve + emit the backlog now that emotes/badges are ready,
                // oldest-first, before any live message (the stream is drained one
                // event at a time).
                if !history_done {
                    history_done = true;
                    emit_history(&meta.name, &registry, &badges, raw_history, &tx).await;
                }
                continue;
            }
            ChatEvent::Message(mut msg) => {
                resolve_message(&mut msg, &registry, &badges);
                spawn_cosmetics(&mut seen_cosmetics, &msg, &tx);
                ChatEvent::Message(msg)
            }
            // A sub/resub's attached message (and everything else) is resolved the
            // same way as the history backlog.
            other => resolve_chat_event(&registry, &badges, other),
        };
        if tx.send(forward).await.is_err() {
            break; // UI side dropped the receiver.
        }
    }
}

/// Applies 3rd-party emote/badge resolution to a chat event the same way for the
/// live stream and the history backlog, so the two never drift: a chat `Message`
/// gets emotes + badges, and so does an `Event`'s attached sub message (it's a
/// full `Message` too, resolved against its own `msg.channel`). Everything else
/// passes through. (The live loop handles `Channel` and cosmetics separately
/// before calling this.)
fn resolve_chat_event(registry: &EmoteRegistry, badges: &BadgeMap, event: ChatEvent) -> ChatEvent {
    match event {
        ChatEvent::Message(mut msg) => {
            resolve_message(&mut msg, registry, badges);
            ChatEvent::Message(msg)
        }
        ChatEvent::Event {
            platform,
            kind,
            text,
            timestamp,
            message,
            details,
        } => ChatEvent::Event {
            platform,
            kind,
            text,
            timestamp,
            message: message.map(|mut msg| {
                resolve_message(&mut msg, registry, badges);
                msg
            }),
            details,
        },
        // A pinned message renders like a chat line in the banner, so it gets
        // the same emote/badge resolution.
        ChatEvent::PinMessage {
            platform,
            pinned_by,
            mut message,
            ends_at,
        } => {
            resolve_message(&mut message, registry, badges);
            ChatEvent::PinMessage {
                platform,
                pinned_by,
                message,
                ends_at,
            }
        }
        other => other,
    }
}

/// Kicks off a one-shot 7TV cosmetics (paint + badge) lookup for `msg`'s author,
/// once per chatter per connection. On a hit it emits a `Cosmetics` event the UI
/// applies to that user's rows. No-op when cosmetics are disabled or the author
/// has no numeric id. The lookup is async + process-cached, so this never blocks
/// the message loop and a chatter costs at most one network round-trip per session.
fn spawn_cosmetics(
    seen: &mut std::collections::HashSet<String>,
    msg: &bks_core::Message,
    tx: &Sink,
) {
    if !bks_emotes::paints_enabled() {
        return;
    }
    let user_id = msg.author.user_id.clone();
    if user_id.is_empty() || !seen.insert(user_id.clone()) {
        return;
    }
    let platform = msg.platform;
    let tx = tx.clone();
    runtime().spawn(async move {
        let cosmetics = bks_emotes::resolve_cosmetics(&user_id).await;
        if cosmetics.is_empty() {
            return;
        }
        let _ = tx
            .send(ChatEvent::Cosmetics {
                platform,
                user_id,
                paint: cosmetics.paint,
                badge: cosmetics.badge,
            })
            .await;
    });
}

/// Applies 3rd-party emotes + badge images to a message (live or history).
fn resolve_message(msg: &mut bks_core::Message, registry: &EmoteRegistry, badges: &BadgeMap) {
    if !registry.is_empty() {
        msg.elements = registry.resolve_elements(&msg.channel, std::mem::take(&mut msg.elements));
    }
    resolve_badges(msg, badges);
}

/// How many recent messages to pull on join.
const HISTORY_LIMIT: usize = 40;

/// Resolves an already-fetched recent-history backlog (emotes/badges, like live
/// messages) and emits it oldest-first. `raw_history` is the result of
/// [`bks_twitch::fetch_recent`], fetched in parallel with emotes/badges by the
/// caller; a fetch failure is logged and skipped (the log just opens empty).
async fn emit_history(
    channel: &str,
    registry: &EmoteRegistry,
    badges: &BadgeMap,
    raw_history: anyhow::Result<Vec<ChatEvent>>,
    tx: &Sink,
) {
    let events = match raw_history {
        Ok(events) => events,
        Err(err) => {
            tracing::warn!("failed to load Twitch history for {channel}: {err:#}");
            return;
        }
    };
    for event in events {
        // Resolve emotes/badges on historical chat messages, just like live ones;
        // a historical sub event's attached message gets the same treatment;
        // clear-chat rows pass through unchanged.
        let event = resolve_chat_event(registry, badges, event);
        if tx.send(event).await.is_err() {
            break; // UI side dropped the receiver.
        }
    }
}

/// Spawns the Twitch channel-point (Hermes) feed for `channel_id`, forwarding its
/// events onto this tab's UI sink. The connector emits into a tokio channel (it
/// has no smol dep); a tiny forwarder drains that into the smol `tx`. Both tasks
/// register with `guard` so they die with the IRC connection that spawned them.
fn spawn_pubsub(channel_id: String, tx: Sink, guard: &mut TaskGuard) {
    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
    guard.add(runtime().spawn(async move {
        // Channel points are nice-to-have: reconnect quietly with backoff on any
        // failure (no error rows), and stop once the UI side is gone.
        let mut attempt: u32 = 0;
        loop {
            let started = std::time::Instant::now();
            let result = bks_twitch::run_pubsub(channel_id.clone(), ptx.clone()).await;
            if ptx.is_closed() {
                break;
            }
            match result {
                Ok(()) => tracing::warn!("twitch points socket closed; reconnecting"),
                Err(err) => tracing::warn!("twitch points: {err:#}; reconnecting"),
            }
            if started.elapsed() > std::time::Duration::from_secs(60) {
                attempt = 0;
            }
            tokio::time::sleep(bks_core::reconnect_delay(attempt)).await;
            attempt += 1;
        }
    }));
    guard.add(runtime().spawn(async move {
        while let Some(event) = prx.recv().await {
            if tx.send(event).await.is_err() {
                break; // UI side dropped the receiver.
            }
        }
    }));
}

/// Registers this tab's channel with the **app-wide shared** EventSub socket
/// (rich moderation notices + AutoMod), forwarding its notifications onto the
/// tab's UI sink. There is now one WebSocket for the whole app (not one per tab)
/// so we stay under Twitch's 3-socket-per-user cap no matter how many channels
/// are open (see [`bks_twitch::register_eventsub`]). The returned registration
/// guard lives on `guard`, so dropping it (IRC connection end) unregisters this
/// channel and frees its subscription slots.
fn spawn_eventsub(auth: EventsubAuth, broadcaster_id: String, tx: Sink, guard: &mut TaskGuard) {
    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<ChatEvent>();
    // The manager owns the socket + reconnect loop; `etx` receives this channel's
    // routed notifications. `None` = the token can't power the feed at all.
    let registration = bks_twitch::register_eventsub(auth, broadcaster_id, etx);
    guard.set_eventsub(registration);
    guard.add(runtime().spawn(async move {
        while let Some(event) = erx.recv().await {
            if tx.send(event).await.is_err() {
                break; // UI side dropped the receiver.
            }
        }
    }));
}

/// Fetches the channel's badge image map (no auth); empty on failure.
async fn load_badges(channel: &str) -> BadgeMap {
    match bks_twitch::fetch_badges(channel).await {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!("failed to load Twitch badges for {channel}: {err:#}");
            BadgeMap::default()
        }
    }
}

/// Fills in each badge's image URL from the map (the connector leaves it empty,
/// storing only the `set/version` id), dropping badges with no known image.
fn resolve_badges(msg: &mut bks_core::Message, badges: &BadgeMap) {
    if badges.is_empty() {
        msg.author.badges.clear();
        return;
    }
    msg.author
        .badges
        .retain_mut(|badge| match badges.url(&badge.id) {
            Some(url) => {
                badge.url = url.to_string();
                badge.title = badges.title(&badge.id).map(str::to_string);
                true
            }
            None => false,
        });
}

/// Adds any emotes in `msg` not already in `seen` to `set` (deduped by name).
/// Returns whether anything new was added (so the caller knows to re-emit). Used
/// to discover Kick native emotes from chat, which have no channel-emote endpoint.
fn harvest_emotes(
    msg: &bks_core::Message,
    set: &mut Vec<bks_core::Emote>,
    seen: &mut std::collections::HashSet<String>,
) -> bool {
    let mut added = false;
    for element in &msg.elements {
        if let bks_core::MessageElement::Emote(emote) = element {
            if seen.insert(emote.name.clone()) {
                set.push((**emote).clone());
                added = true;
            }
        }
    }
    added
}

/// Sends a platform's current emote set to the UI for its picker tab.
async fn emit_emotes(platform: bks_core::Platform, emotes: &[bks_core::Emote], tx: &Sink) {
    let _ = tx
        .send(ChatEvent::Emotes {
            platform,
            emotes: emotes.to_vec(),
        })
        .await;
}

/// Drives the Kick connection: resolves 7TV emotes (Kick's own emotes arrive
/// already inline-parsed, so resolution only fills in remaining text runs) and
/// records each chatter's numeric id with the controller so moderation can
/// target them (Kick's API can't resolve a username → id).
async fn run_kick(source: Arc<KickSource>, channel: String, tx: Sink, controller: Controller) {
    let emote_providers = kick_providers();
    // Globals are shared app-wide (loaded once); this registry only adds the
    // channel's own emotes on top.
    let mut registry = EmoteRegistry::with_globals(kick_globals().await);
    // The Kick channel's emote set for the picker. Kick has no channel-emote
    // endpoint wired, so this is built from two sources: the 7TV set loaded on
    // join, plus native Kick emotes (`[emote:id:name]`) discovered as messages
    // arrive (deduped by name). When it grows, the fresh set is re-emitted.
    let mut kick_emotes: Vec<bks_core::Emote> = Vec::new();
    let mut seen_emote_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut stream = match source.join(&channel).await {
        Ok(stream) => stream,
        Err(err) => {
            let _ = tx
                .send(ChatEvent::Error(format!("kick failed: {err:#}")))
                .await;
            return;
        }
    };

    while let Some(event) = stream.recv().await {
        let forward = match event {
            // Kick can't resolve a username → id, so remember each chatter's id
            // for moderation, and fill in their standard badge images.
            ChatEvent::Message(mut msg) => {
                if let Ok(id) = msg.author.user_id.parse::<u64>() {
                    controller.note_kick_user(msg.author.login.clone(), id);
                }
                // Our own messages carry our badges — the freshest mod signal.
                controller.sync_kick_mod_from_message(&msg);
                if !registry.is_empty() {
                    msg.elements = registry.resolve_elements(&msg.channel, msg.elements);
                }
                resolve_kick_badges(&mut msg);
                // Harvest any native Kick emotes in this message into the picker
                // set; re-emit when something new shows up so the picker/completion
                // pick them up live.
                if harvest_emotes(&msg, &mut kick_emotes, &mut seen_emote_names) {
                    emit_emotes(bks_core::Platform::Kick, &kick_emotes, &tx).await;
                }
                ChatEvent::Message(msg)
            }
            // Channel identity isn't shown in chat (Kick login uses its own
            // flow); use it to load this channel's emotes for the picker's Kick
            // tab — 7TV (via providers) plus the channel's *native* Kick emotes
            // (fetched directly from Cloudflare-fronted kick.com via the emulated
            // client) — then drop it. The two run concurrently.
            ChatEvent::Channel(meta) => {
                let native_fut = bks_kick::fetch_channel_emotes(&meta.name);
                let seventv_fut = load_emotes(&mut registry, &emote_providers, &meta);
                let (_, native) = tokio::join!(seventv_fut, native_fut);

                for emote in registry.emotes(&meta.name) {
                    if seen_emote_names.insert(emote.name.clone()) {
                        kick_emotes.push((*emote).clone());
                    }
                }
                match native {
                    Ok(natives) => {
                        for emote in natives {
                            if seen_emote_names.insert(emote.name.clone()) {
                                kick_emotes.push(emote);
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            "failed to load native Kick emotes for {}: {err:#}",
                            meta.name
                        )
                    }
                }
                emit_emotes(bks_core::Platform::Kick, &kick_emotes, &tx).await;
                continue;
            }
            // A pinned message renders like a chat line in the banner: resolve
            // its 7TV emotes and fill in the standard badge images, same as a
            // live message.
            ChatEvent::PinMessage {
                platform,
                pinned_by,
                mut message,
                ends_at,
            } => {
                if !registry.is_empty() {
                    message.elements =
                        registry.resolve_elements(&message.channel, message.elements);
                }
                resolve_kick_badges(&mut message);
                ChatEvent::PinMessage {
                    platform,
                    pinned_by,
                    message,
                    ends_at,
                }
            }
            other => other,
        };
        if tx.send(forward).await.is_err() {
            break;
        }
    }
}

/// Drives the YouTube connection: like Kick, YouTube's own (custom channel)
/// emojis arrive already inline-parsed, so 7TV resolution only fills remaining
/// text runs. On the `Channel` event (which carries the owner `UC…` id) we load
/// 7TV for that channel and emit the picker set; native YouTube emojis are
/// harvested from messages as they arrive (there's no channel-emoji endpoint).
async fn run_youtube(source: Arc<YouTubeSource>, channel: String, tx: Sink) {
    let emote_providers = youtube_providers();
    let mut registry = EmoteRegistry::with_globals(youtube_globals().await);
    let mut yt_emotes: Vec<bks_core::Emote> = Vec::new();
    let mut seen_emote_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut stream = match source.join(&channel).await {
        Ok(stream) => stream,
        Err(err) => {
            let _ = tx
                .send(ChatEvent::Error(format!("youtube failed: {err:#}")))
                .await;
            return;
        }
    };

    while let Some(event) = stream.recv().await {
        let forward = match event {
            ChatEvent::Message(mut msg) => {
                if !registry.is_empty() {
                    msg.elements = registry.resolve_elements(&msg.channel, msg.elements);
                }
                if harvest_emotes(&msg, &mut yt_emotes, &mut seen_emote_names) {
                    emit_emotes(bks_core::Platform::YouTube, &yt_emotes, &tx).await;
                }
                ChatEvent::Message(msg)
            }
            // Channel identity carries the owner `UC…` id; load its 7TV set for the
            // picker's YouTube tab, then drop it (not shown in chat).
            ChatEvent::Channel(meta) => {
                load_emotes(&mut registry, &emote_providers, &meta).await;
                for emote in registry.emotes(&meta.name) {
                    if seen_emote_names.insert(emote.name.clone()) {
                        yt_emotes.push((*emote).clone());
                    }
                }
                emit_emotes(bks_core::Platform::YouTube, &yt_emotes, &tx).await;
                continue;
            }
            other => other,
        };
        if tx.send(forward).await.is_err() {
            break;
        }
    }
}

/// Fills Kick standard badges with their bundled asset path and drops any with no
/// known image. Subscriber badges already carry a CDN url (filled by the connector
/// from channel resolution), so they're kept as-is.
fn resolve_kick_badges(msg: &mut bks_core::Message) {
    msg.author.badges.retain_mut(|badge| {
        if !badge.url.is_empty() {
            return true; // Already resolved (subscriber tier image).
        }
        match crate::assets::kick_badge_path(&badge.id) {
            Some(path) => {
                badge.url = path.to_string();
                true
            }
            None => false,
        }
    });
}

/// The 3rd-party emote providers for Twitch, in priority order (earlier wins
/// name collisions): FFZ, then BTTV, then 7TV. All are Twitch-only here.
fn providers() -> Vec<Box<dyn EmoteProvider>> {
    vec![
        Box::new(FfzProvider::new()),
        Box::new(BttvProvider::new()),
        Box::new(SeventvProvider::new()),
    ]
}

/// The 3rd-party emote providers for Kick, in priority order. Same set as Twitch
/// but each must resolve channel emotes by Kick id (7TV's `/users/kick/` path).
fn kick_providers() -> Vec<Box<dyn EmoteProvider>> {
    vec![Box::new(SeventvProvider::for_kick())]
}

/// The 3rd-party emote providers for YouTube: 7TV keyed on the channel's `UC…` id
/// (7TV's `/users/youtube/` path). YouTube's own emojis arrive inline already.
fn youtube_providers() -> Vec<Box<dyn EmoteProvider>> {
    vec![Box::new(SeventvProvider::for_youtube())]
}

/// The shared, app-wide global emote sets (one per platform's provider list).
/// Loaded at most once each — every tab's registry points at the same `Arc`
/// instead of copying the (identical) globals, so opening N tabs no longer
/// reloads/clones the global sets N times (the source of the startup log spam).
type GlobalMap = std::sync::Arc<bks_emotes::EmoteMap>;

/// Returns the shared Twitch global set, loading it once on first use.
async fn twitch_globals() -> GlobalMap {
    static CELL: tokio::sync::OnceCell<GlobalMap> = tokio::sync::OnceCell::const_new();
    CELL.get_or_init(|| load_globals(providers())).await.clone()
}

/// Returns the shared Kick global set, loading it once on first use.
async fn kick_globals() -> GlobalMap {
    static CELL: tokio::sync::OnceCell<GlobalMap> = tokio::sync::OnceCell::const_new();
    CELL.get_or_init(|| load_globals(kick_providers()))
        .await
        .clone()
}

/// Returns the shared YouTube global set, loading it once on first use.
async fn youtube_globals() -> GlobalMap {
    static CELL: tokio::sync::OnceCell<GlobalMap> = tokio::sync::OnceCell::const_new();
    CELL.get_or_init(|| load_globals(youtube_providers()))
        .await
        .clone()
}

/// Builds a shared global map by loading every provider's global set once.
async fn load_globals(providers: Vec<Box<dyn EmoteProvider>>) -> GlobalMap {
    let mut registry = EmoteRegistry::new();
    registry.load_globals(&providers).await;
    registry.globals()
}

/// Unwraps interned (`Arc`) emotes into owned ones for the picker payload
/// (`ChatEvent::Emotes`), the UI's one-per-channel snapshot — the `Arc` interning
/// is for the per-frame message path, not this.
fn owned_emotes(emotes: Vec<std::sync::Arc<bks_core::Emote>>) -> Vec<bks_core::Emote> {
    emotes.into_iter().map(|e| (*e).clone()).collect()
}

/// Loads the channel's per-channel emotes into `registry` (globals already come
/// shared) and logs how many providers resolved (status, not shown in chat).
async fn load_emotes(
    registry: &mut EmoteRegistry,
    providers: &[Box<dyn EmoteProvider>],
    meta: &ChannelMeta,
) {
    let loaded = registry
        .load_providers(providers, &meta.name, &meta.id)
        .await;
    tracing::info!(
        "emotes loaded for {} ({loaded}/{} providers)",
        meta.name,
        providers.len()
    );
}
