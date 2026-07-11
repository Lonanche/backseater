//! A tab's handle for acting on its chat: sending, moderation, and connecting.
//!
//! There is one Controller per tab. Login is app-wide (see [`Session`]); a
//! Controller borrows the shared session for credentials/actions but owns this
//! tab's connection (the Twitch source), its channels, send target, and the Kick
//! chatters it has seen. The GPUI side calls these methods; each spawns onto the
//! shared tokio runtime and reports outcomes back as `System` events in this
//! tab's feed.
//!
//! Sending is routed by a per-tab [`SendTarget`] (Twitch / Kick / Both). `/login`
//! etc. delegate to the shared session; channel-targeting actions use this tab's
//! channels.

use std::collections::HashMap;
use std::sync::Arc;

use bks_kick::{KickActions, KickUserInfo};
use bks_platform::{ChatEvent, ChatSource};
use bks_twitch::{TwitchActions, TwitchSource, TwitchUserCard};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use crate::session::{self, Session};

/// The message a pending reply points at: which platform + the parent's id, plus
/// the parent author/body so a reply's local echo (Twitch) can show the same
/// "replying to" line. Built by the UI when the user clicks a row's reply button.
#[derive(Clone)]
pub struct ReplyTo {
    pub platform: bks_core::Platform,
    pub message_id: String,
    pub parent: bks_core::ReplyParent,
    /// The parent's renderable tokens (text + emotes), so the "replying to" bar can
    /// show its emotes inline. Not sent — only the id + author/text reach the wire.
    pub parent_elements: Vec<bks_core::MessageElement>,
}

/// Which platform(s) typed messages + commands go to. Cycled by the UI toggle.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum SendTarget {
    #[default]
    Twitch,
    Kick,
    Both,
}

/// A moderation request parsed from a `/command`.
enum ModAction {
    Ban {
        user: String,
        reason: Option<String>,
    },
    Timeout {
        user: String,
        secs: u32,
    },
    Unban {
        user: String,
    },
    Delete {
        message_id: String,
    },
}

/// A Twitch role change (grant/revoke moderator or VIP), driven by the usercard's
/// buttons. Separate from [`ModAction`] because these are owner-only Helix calls
/// (not part of the ban family) and Twitch-only.
#[derive(Clone, Copy)]
pub enum Role {
    Moderator,
    Vip,
}

/// This tab's mutable connection state. Behind a mutex so the GPUI side and
/// tokio tasks can share it.
#[derive(Default)]
struct State {
    /// Active Twitch source for this tab — anonymous or authed. Cancelled when
    /// swapped (reconnect / channel change) so we never run two at once.
    twitch: Option<Arc<TwitchSource>>,
    /// Kick chatters this tab has seen (login → numeric id), so moderation can
    /// target them — Kick's API can't resolve a username to an id.
    kick_user_ids: HashMap<String, u64>,
}

/// A tab's send/moderate/connect handle. Clone is cheap.
#[derive(Clone)]
pub struct Controller {
    state: Arc<Mutex<State>>,
    /// Where typed messages go (per tab). A sync mutex (never held across an
    /// await) so the UI's render-path read can't silently miss under contention
    /// — the old `try_lock().unwrap_or_default()` briefly showed the wrong
    /// target when a tokio task held the state lock.
    target: Arc<std::sync::Mutex<SendTarget>>,
    /// App-wide login (shared across tabs).
    session: Session,
    /// Posts results back into this tab's feed as system notices.
    events: smol::channel::Sender<ChatEvent>,
    rt: Handle,
    /// This tab's Twitch channel (login, no `#`), empty if none.
    twitch_channel: String,
    /// This tab's Kick channel (slug), empty if none.
    kick_channel: String,
}

impl Controller {
    pub fn new(
        session: Session,
        events: smol::channel::Sender<ChatEvent>,
        rt: Handle,
        twitch_channel: String,
        kick_channel: String,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            target: Arc::new(std::sync::Mutex::new(SendTarget::default())),
            session,
            events,
            rt,
            twitch_channel,
            kick_channel,
        }
    }

    /// Connects this tab (authed if logged in, else anonymous) and subscribes to
    /// app-wide login changes so it stays in sync no matter what caused them
    /// (user logout, token expiry, login from a future settings screen, …). Call
    /// once when the tab connects.
    pub fn start(&self) {
        // Do the initial connect and then react to every login change through the
        // same path — one cause-agnostic loop, so a change racing with startup
        // can't be missed and later-opened tabs stay consistent too.
        self.watch_login_changes();
    }

    /// Re-joins this tab's Twitch channel with the session's current auth.
    async fn reconnect_twitch(&self) {
        if self.twitch_channel.is_empty() {
            return;
        }
        let auth = self.session.twitch_auth().await;
        self.connect_twitch(session::twitch_source(auth)).await;
    }

    /// Subscribes to the session's login broadcast and keeps this tab in sync.
    /// Connects once up front, then on each change reconnects Twitch (so its
    /// read/IRC source matches the current auth) and, if Kick became logged out,
    /// resets the send target off Kick.
    fn watch_login_changes(&self) {
        let this = self.clone();
        let mut rx = self.session.subscribe();
        self.rt.spawn(async move {
            let mut prev = rx.borrow().clone();
            // Initial connect with whatever's logged in right now.
            this.reconnect_twitch().await;
            while rx.changed().await.is_ok() {
                // The session outlives every tab, so `changed()` alone would keep
                // this task (and a reconnect per login flip) alive forever after
                // the tab closed. A closed events channel means the ChatView is
                // gone — stop reconciling.
                if this.events.is_closed() {
                    break;
                }
                let now = rx.borrow().clone();
                if now.twitch() != prev.twitch() {
                    this.reconnect_twitch().await;
                }
                if prev.kick() && !now.kick() {
                    this.reset_target_off_kick();
                }
                prev = now;
            }
        });
    }

    /// On Kick logout, a tab targeting Kick/Both can no longer send there, so
    /// fall back to Twitch.
    fn reset_target_off_kick(&self) {
        let mut target = self.target.lock().unwrap();
        if matches!(*target, SendTarget::Kick | SendTarget::Both) {
            *target = SendTarget::Twitch;
        }
    }

    /// Records a Kick chatter's id (called by the bridge as messages arrive) so
    /// `/ban`/`/timeout` on Kick can resolve the target.
    pub fn note_kick_user(&self, login: String, user_id: u64) {
        let state = self.state.clone();
        self.rt.spawn(async move {
            state.lock().await.kick_user_ids.insert(login, user_id);
        });
    }

    /// Account actions for the settings UI. Each only mutates the shared
    /// [`Session`]; every tab reacts via the login-change subscription. Progress
    /// and error notices for an attempt go to this tab's feed.
    pub fn twitch_login(&self) {
        self.session.twitch_login(self.events.clone());
    }

    pub fn twitch_logout(&self) {
        self.session.twitch_logout();
    }

    pub fn kick_login(&self) {
        self.session.kick_login(self.events.clone());
    }

    pub fn kick_logout(&self) {
        self.session.kick_logout();
    }

    /// The send target, for the UI toggle to render.
    pub fn send_target(&self) -> SendTarget {
        *self.target.lock().unwrap()
    }

    /// Whether the user is logged into Kick (controls whether the toggle shows).
    pub fn kick_logged_in(&self) -> bool {
        self.session.kick_logged_in()
    }

    /// Whether this tab has a Kick channel set (the toggle is only useful then).
    pub fn has_kick(&self) -> bool {
        !self.kick_channel.is_empty()
    }

    /// Cycles the send target Twitch → Kick → Both (only meaningful when logged
    /// into Kick and this tab has a Kick channel).
    pub fn cycle_send_target(&self) {
        if !self.kick_logged_in() || !self.has_kick() {
            return;
        }
        let mut target = self.target.lock().unwrap();
        *target = match *target {
            SendTarget::Twitch => SendTarget::Kick,
            SendTarget::Kick => SendTarget::Both,
            SendTarget::Both => SendTarget::Twitch,
        };
        let label = match *target {
            SendTarget::Twitch => "Twitch",
            SendTarget::Kick => "Kick",
            SendTarget::Both => "Twitch + Kick",
        };
        tracing::info!("send target: {label}");
    }

    /// Reports a user-facing error in this tab's feed (a copyable error row).
    fn notice(&self, msg: impl Into<String>) {
        let _ = self.events.try_send(ChatEvent::Error(msg.into()));
    }

    /// The Twitch actions client, posting the "log in first" hint when logged
    /// out — the shared guard of every authed Twitch method here.
    async fn twitch_actions_or_hint(&self) -> Option<Arc<TwitchActions>> {
        let actions = self.session.twitch_actions().await;
        if actions.is_none() {
            self.notice("log into Twitch first: /login");
        }
        actions
    }

    /// The Kick actions client, posting the "log in first" hint when logged out.
    async fn kick_actions_or_hint(&self) -> Option<Arc<KickActions>> {
        let actions = self.session.kick_actions().await;
        if actions.is_none() {
            self.notice("log into Kick first: /kicklogin");
        }
        actions
    }

    /// Whether this tab has a Twitch channel, posting the explanatory notice
    /// when it doesn't.
    fn require_twitch_channel(&self) -> bool {
        if self.twitch_channel.is_empty() {
            self.notice("this tab has no Twitch channel");
            return false;
        }
        true
    }

    /// Whether this tab has a Kick channel, posting the explanatory notice
    /// when it doesn't.
    fn require_kick_channel(&self) -> bool {
        if self.kick_channel.is_empty() {
            self.notice("this tab has no Kick channel");
            return false;
        }
        true
    }

    /// Bans `user` on Twitch directly (used by the per-message mod buttons, which
    /// act on the row's platform regardless of the current send target).
    pub fn ban_twitch(&self, user: String) {
        self.spawn_twitch_mod(ModAction::Ban { user, reason: None });
    }

    /// Times `user` out on Twitch for `secs` seconds (per-message mod button).
    pub fn timeout_twitch(&self, user: String, secs: u32) {
        self.spawn_twitch_mod(ModAction::Timeout { user, secs });
    }

    /// Lifts a ban/timeout on `user` on Twitch (usercard Unban button).
    pub fn unban_twitch(&self, user: String) {
        self.spawn_twitch_mod(ModAction::Unban { user });
    }

    /// Allows (`true`) or denies a message AutoMod is holding for review (the
    /// held row's Allow/Deny buttons). The result lands back as an EventSub
    /// `automod.message.update`, which resolves the row in place.
    pub fn automod_twitch(&self, message_id: String, allow: bool) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.twitch_actions_or_hint().await else {
                return;
            };
            if let Err(err) = actions.automod_message(&message_id, allow).await {
                this.notice(format!("{err:#}"));
            }
        });
    }

    /// Grants (`grant`) or revokes a Twitch [`Role`] for `user` (usercard
    /// mod/VIP buttons). Owner-only on Twitch's side; a non-owner attempt reports
    /// the API error as a notice.
    pub fn set_role_twitch(&self, role: Role, grant: bool, user: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.twitch_actions_or_hint().await else {
                return;
            };
            if !this.require_twitch_channel() {
                return;
            }
            let ch = &this.twitch_channel;
            let result = match (role, grant) {
                (Role::Moderator, true) => actions.add_moderator(ch, &user).await,
                (Role::Moderator, false) => actions.remove_moderator(ch, &user).await,
                (Role::Vip, true) => actions.add_vip(ch, &user).await,
                (Role::Vip, false) => actions.remove_vip(ch, &user).await,
            };
            if let Err(err) = result {
                this.notice(format!("{err:#}"));
            }
        });
    }

    /// Fetches the Twitch usercard for `login` (account info + follow age for this
    /// tab's channel) and delivers the result over `reply`. Requires Twitch login;
    /// sends `Err` describing why if not authed or the lookup fails.
    pub fn fetch_twitch_usercard(
        &self,
        login: String,
        reply: smol::channel::Sender<anyhow::Result<TwitchUserCard>>,
    ) {
        let this = self.clone();
        self.rt.spawn(async move {
            let result = match this.session.twitch_actions().await {
                Some(actions) => actions.usercard(&login, &this.twitch_channel).await,
                None => Err(anyhow::anyhow!("log into Twitch to see account details")),
            };
            let _ = reply.send(result).await;
        });
    }

    /// Fetches this tab's Twitch viewer list (Helix chatters — broadcaster/mod
    /// only) and delivers the result over `reply`. Sends `Err` describing why
    /// when not logged in or when Twitch refuses (not a mod, old token).
    pub fn fetch_twitch_chatters(
        &self,
        reply: smol::channel::Sender<anyhow::Result<bks_twitch::Chatters>>,
    ) {
        let this = self.clone();
        self.rt.spawn(async move {
            let result = match this.session.twitch_actions().await {
                Some(actions) => actions.chatters(&this.twitch_channel).await,
                None => Err(anyhow::anyhow!(
                    "log into Twitch (/login) to fetch the viewer list — Twitch \
                     only shows it to the broadcaster and moderators"
                )),
            };
            let _ = reply.send(result).await;
        });
    }

    /// Fetches the Kick usercard for `login` (this chatter's standing in this
    /// tab's channel — follow date, sub months, mod flag) and delivers the result
    /// over `reply`. Unauthenticated: the lookup hits Kick's Cloudflare-fronted
    /// endpoint directly via the emulated client, so it works whether or not we're
    /// logged into Kick.
    pub fn fetch_kick_usercard(
        &self,
        login: String,
        reply: smol::channel::Sender<anyhow::Result<KickUserInfo>>,
    ) {
        let channel = self.kick_channel.clone();
        self.rt.spawn(async move {
            let result = bks_kick::fetch_user_info(&channel, &login).await;
            let _ = reply.send(result).await;
        });
    }

    /// Fetches the logged-in user's personal Twitch emotes (sub/follower/global)
    /// for the emote picker, delivering the result over `reply`. Sends an empty
    /// list when not logged into Twitch (the picker then shows only 7TV emotes).
    pub fn fetch_twitch_emotes(&self, reply: smol::channel::Sender<Vec<bks_core::Emote>>) {
        let this = self.clone();
        self.rt.spawn(async move {
            let emotes = match this.session.twitch_actions().await {
                Some(actions) => {
                    // The user's own usable emotes plus the viewed channel's native
                    // set (sub/follower/bits) — the latter so channel emotes like a
                    // sub emote show in the picker even when we're not subscribed
                    // (native emotes bypass the 3rd-party registry, so they're not
                    // in the bridge's Emotes payload). Fetched concurrently; a
                    // failure of either just yields fewer emotes.
                    let channel = this.twitch_channel.clone();
                    let (personal, channel_emotes) = tokio::join!(
                        actions.user_emotes(),
                        async {
                            if channel.is_empty() {
                                Ok(Vec::new())
                            } else {
                                actions.channel_emotes(&channel).await
                            }
                        }
                    );
                    let mut emotes = personal.unwrap_or_default();
                    let mut seen: std::collections::HashSet<String> =
                        emotes.iter().map(|e| e.name.clone()).collect();
                    for e in channel_emotes.unwrap_or_default() {
                        if seen.insert(e.name.clone()) {
                            emotes.push(e);
                        }
                    }
                    emotes
                }
                None => Vec::new(),
            };
            let _ = reply.send(emotes).await;
        });
    }

    /// Runs a Twitch moderation action, reporting "log in first" when not authed.
    fn spawn_twitch_mod(&self, action: ModAction) {
        let this = self.clone();
        self.rt.spawn(async move {
            if let Some(a) = this.twitch_actions_or_hint().await {
                this.moderate_twitch(&a, action).await;
            }
        });
    }

    /// Bans `user` on Kick (usercard Ban button — acts on the Kick chatter
    /// regardless of the tab's send target).
    pub fn ban_kick(&self, user: String) {
        self.spawn_kick_mod(ModAction::Ban { user, reason: None });
    }

    /// Times `user` out on Kick for `secs` seconds (usercard preset button).
    pub fn timeout_kick(&self, user: String, secs: u32) {
        self.spawn_kick_mod(ModAction::Timeout { user, secs });
    }

    /// Lifts a Kick ban/timeout on `user` (usercard Unban button).
    pub fn unban_kick(&self, user: String) {
        self.spawn_kick_mod(ModAction::Unban { user });
    }

    /// Default pin duration (seconds) for the hover pin button — the web
    /// clients' usual 20 minutes (Twitch allows 30–1800s, Kick sends 1200).
    const PIN_DURATION_SECS: u32 = 1200;

    /// Pins the Twitch message `message_id` in this tab's channel (per-row 📌
    /// button; needs to moderate the channel — a 403 surfaces as the error row).
    pub fn pin_twitch(&self, message_id: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.twitch_actions_or_hint().await else {
                return;
            };
            if !this.require_twitch_channel() {
                return;
            }
            if let Err(err) = actions
                .pin_message(
                    &this.twitch_channel,
                    &message_id,
                    Some(Self::PIN_DURATION_SECS),
                )
                .await
            {
                this.notice(format!("{err:#}"));
            }
        });
    }

    /// Unpins the Twitch pinned message `message_id` (the banner's Unpin button).
    pub fn unpin_twitch(&self, message_id: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.twitch_actions_or_hint().await else {
                return;
            };
            if !this.require_twitch_channel() {
                return;
            }
            if let Err(err) = actions
                .unpin_message(&this.twitch_channel, &message_id)
                .await
            {
                this.notice(format!("{err:#}"));
            }
        });
    }

    /// Pins a Kick message in this tab's channel (per-row 📌 button). `msg`
    /// carries the original message's fields — Kick's pin endpoint wants the
    /// whole message object back, not just an id.
    pub fn pin_kick(&self, msg: bks_kick::PinnableMessage) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.kick_actions_or_hint().await else {
                return;
            };
            if !this.require_kick_channel() {
                return;
            }
            if let Err(err) = actions
                .pin_message(&this.kick_channel, &msg, Self::PIN_DURATION_SECS as u64)
                .await
            {
                this.report_kick_error(&err);
            }
        });
    }

    /// Unpins this tab's Kick pinned message (the banner's Unpin button — Kick
    /// keys the unpin on the channel, no message id needed).
    pub fn unpin_kick(&self) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.kick_actions_or_hint().await else {
                return;
            };
            if !this.require_kick_channel() {
                return;
            }
            if let Err(err) = actions.unpin_message(&this.kick_channel).await {
                this.report_kick_error(&err);
            }
        });
    }

    /// Runs a Kick moderation action, reporting "log in first" when not authed.
    fn spawn_kick_mod(&self, action: ModAction) {
        let this = self.clone();
        self.rt.spawn(async move {
            if let Some(a) = this.kick_actions_or_hint().await {
                this.moderate_kick(&a, action).await;
            }
        });
    }

    /// Parses a UI input line: a `/command` or plain chat text to send. When
    /// `reply` is set the line is sent as a threaded reply to that message (on the
    /// parent's platform), ignoring the send-target toggle; `/commands` never reply.
    pub fn handle_input(&self, line: &str, reply: Option<ReplyTo>) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if let Some(rest) = line.strip_prefix('/') {
            self.handle_command(rest);
        } else if let Some(reply) = reply {
            self.send_reply(line.to_string(), reply);
        } else {
            self.send(line.to_string());
        }
    }

    fn handle_command(&self, rest: &str) {
        let mut parts = rest.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let args: Vec<&str> = parts.collect();
        match cmd {
            // Login/logout only mutate the shared Session; every tab (including
            // this one) reacts via the login-change subscription in `start`.
            "login" => self.session.twitch_login(self.events.clone()),
            "logout" => self.session.twitch_logout(),
            "kicklogin" => self.session.kick_login(self.events.clone()),
            "kicklogout" => self.session.kick_logout(),
            "ban" => match args.split_first() {
                Some((user, reason)) => self.moderate(ModAction::Ban {
                    user: user.to_string(),
                    reason: join(reason),
                }),
                None => self.notice("usage: /ban <user> [reason]"),
            },
            // Timeout is in seconds here; converted to minutes for Kick downstream.
            "timeout" => match args.as_slice() {
                [user, secs, ..] => match secs.parse::<u32>() {
                    Ok(secs) => self.moderate(ModAction::Timeout {
                        user: user.to_string(),
                        secs,
                    }),
                    Err(_) => self.notice("usage: /timeout <user> <seconds>"),
                },
                _ => self.notice("usage: /timeout <user> <seconds>"),
            },
            "unban" | "untimeout" => match args.first() {
                Some(user) => self.moderate(ModAction::Unban {
                    user: user.to_string(),
                }),
                None => self.notice("usage: /unban <user>"),
            },
            "delete" => match args.first() {
                Some(id) => self.moderate(ModAction::Delete {
                    message_id: id.to_string(),
                }),
                None => self.notice("usage: /delete <message-id>"),
            },
            other => self.notice(format!("unknown command: /{other}")),
        }
    }

    fn send(&self, text: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            let target = this.send_target();
            let twitch = this.state.lock().await.twitch.clone();
            if matches!(target, SendTarget::Twitch | SendTarget::Both)
                && !this.twitch_channel.is_empty()
            {
                if let Some(src) = twitch {
                    if let Err(err) = src.send(&this.twitch_channel, &text, None).await {
                        this.notice(format!("twitch: {err:#}"));
                    }
                }
            }
            if matches!(target, SendTarget::Kick | SendTarget::Both)
                && !this.kick_channel.is_empty()
            {
                match this.session.kick_actions().await {
                    Some(actions) => this.send_kick(&actions, &text, None).await,
                    None => this.notice("not logged into Kick (/kicklogin)"),
                }
            }
        });
    }

    /// Sends `text` as a threaded reply to `reply.message_id` on the parent's
    /// platform (ignoring the send-target toggle — you reply where the message is).
    fn send_reply(&self, text: String, reply: ReplyTo) {
        let this = self.clone();
        self.rt.spawn(async move {
            match reply.platform {
                bks_core::Platform::Twitch => {
                    if this.twitch_channel.is_empty() {
                        return this.notice("this tab has no Twitch channel");
                    }
                    let src = this.state.lock().await.twitch.clone();
                    match src {
                        Some(src) => {
                            if let Err(err) = src
                                .send_reply(
                                    &this.twitch_channel,
                                    &text,
                                    &reply.message_id,
                                    reply.parent,
                                )
                                .await
                            {
                                this.notice(format!("twitch: {err:#}"));
                            }
                        }
                        None => this.notice("log into Twitch to reply: /login"),
                    }
                }
                bks_core::Platform::Kick => match this.session.kick_actions().await {
                    Some(actions) => {
                        this.send_kick(&actions, &text, Some(&reply.message_id))
                            .await
                    }
                    None => this.notice("log into Kick to reply: /kicklogin"),
                },
                _ => this.notice("replies aren't supported on this platform"),
            }
        });
    }

    /// Kick send: resolve the broadcaster id then POST the message (optionally as a
    /// reply to `reply_to`).
    async fn send_kick(&self, actions: &KickActions, text: &str, reply_to: Option<&str>) {
        match actions.broadcaster_id(&self.kick_channel).await {
            Ok(id) => {
                if let Err(err) = actions.send(id, text, reply_to).await {
                    self.report_kick_error(&err);
                }
            }
            Err(err) => self.report_kick_error(&err),
        }
    }

    /// Reports a Kick action error as a notice, and — when the token has truly
    /// expired (refresh failed) — logs out of Kick so the UI stops claiming we're
    /// logged in and every tab falls back to anonymous (via the login broadcast).
    fn report_kick_error(&self, err: &anyhow::Error) {
        self.notice(format!("kick: {err:#}"));
        if err.downcast_ref::<bks_kick::AuthExpired>().is_some() {
            self.session.kick_logout();
        }
    }

    /// Runs a moderation action on this tab's current single-platform target.
    /// Disabled in `Both` mode; reports "log in first" when not authed.
    fn moderate(&self, action: ModAction) {
        let this = self.clone();
        self.rt.spawn(async move {
            let target = this.send_target();
            match target {
                SendTarget::Both => this.notice("switch to one platform to moderate"),
                SendTarget::Twitch => {
                    if let Some(a) = this.twitch_actions_or_hint().await {
                        this.moderate_twitch(&a, action).await;
                    }
                }
                SendTarget::Kick => {
                    if let Some(a) = this.kick_actions_or_hint().await {
                        this.moderate_kick(&a, action).await;
                    }
                }
            }
        });
    }

    async fn moderate_twitch(&self, actions: &TwitchActions, action: ModAction) {
        if !self.require_twitch_channel() {
            return;
        }
        let ch = &self.twitch_channel;
        let result = match action {
            ModAction::Ban { user, reason } => actions.ban(ch, &user, reason.as_deref()).await,
            ModAction::Timeout { user, secs } => actions.timeout(ch, &user, secs).await,
            ModAction::Unban { user } => actions.unban(ch, &user).await,
            ModAction::Delete { message_id } => actions.delete_message(ch, &message_id).await,
        };
        if let Err(err) = result {
            self.notice(format!("{err:#}"));
        }
    }

    async fn moderate_kick(&self, actions: &KickActions, action: ModAction) {
        if !self.require_kick_channel() {
            return;
        }
        // Kick has no delete endpoint; ban/timeout/unban all need numeric ids
        // resolved from chatters we've seen (its API can't look up a user by name).
        let user = match &action {
            ModAction::Ban { user, .. }
            | ModAction::Timeout { user, .. }
            | ModAction::Unban { user } => user.clone(),
            ModAction::Delete { .. } => return self.notice("Kick has no delete-message API"),
        };
        let target_id = {
            let s = self.state.lock().await;
            s.kick_user_ids.get(&user.to_lowercase()).copied()
        };
        let Some(target_id) = target_id else {
            self.notice(format!(
                "haven't seen '{user}' in Kick chat yet — can't resolve id"
            ));
            return;
        };
        let broadcaster_id = match actions.broadcaster_id(&self.kick_channel).await {
            Ok(id) => id,
            Err(err) => return self.report_kick_error(&err),
        };
        let result = match action {
            ModAction::Ban { reason, .. } => {
                actions
                    .ban(broadcaster_id, target_id, None, reason.as_deref())
                    .await
            }
            // Twitch /timeout is seconds; Kick wants minutes (round up, min 1).
            ModAction::Timeout { secs, .. } => {
                let minutes = secs.div_ceil(60).max(1);
                actions
                    .ban(broadcaster_id, target_id, Some(minutes), None)
                    .await
            }
            ModAction::Unban { .. } => actions.unban(broadcaster_id, target_id).await,
            ModAction::Delete { .. } => unreachable!("handled above"),
        };
        if let Err(err) = result {
            self.report_kick_error(&err);
        }
    }

    /// Replaces this tab's Twitch connection: cancels the old source (closing its
    /// socket + read task) and starts a fresh `run_twitch` for the new one. The
    /// session's EventSub credentials (if logged in) ride along so the moderator
    /// feed connects as this user; a login flip lands back here with the new ones.
    async fn connect_twitch(&self, source: Arc<TwitchSource>) {
        if let Some(old) = self.state.lock().await.twitch.replace(source.clone()) {
            old.cancel();
        }
        let eventsub = self.session.twitch_eventsub().await;
        self.rt.spawn(crate::bridge::run_twitch(
            source,
            format!("#{}", self.twitch_channel),
            self.events.clone(),
            eventsub,
        ));
    }
}

fn join(parts: &[&str]) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}
