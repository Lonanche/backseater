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

use crate::emote_cache;
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

impl SendTarget {
    /// The human label used in the "send target: X" status line.
    fn label(self) -> &'static str {
        match self {
            SendTarget::Twitch => "Twitch",
            SendTarget::Kick => "Kick",
            SendTarget::Both => "Twitch + Kick",
        }
    }
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
        reason: Option<String>,
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

/// A Twitch-only slash command beyond the shared ban family, parsed in
/// [`Controller::handle_command`] and run against Helix by
/// [`Controller::twitch_cmd`]. Chat-mode changes show up via ROOMSTATE (the
/// mode bar) and announcements/clears in chat, so only the variants with no
/// visible effect post a confirmation notice.
enum TwitchCmd {
    Announce {
        message: String,
        /// blue/green/orange/purple; `None` = the channel's accent color.
        color: Option<&'static str>,
    },
    Warn {
        user: String,
        reason: String,
    },
    /// twitch.tv's `/pin`: send the message, then pin it for `duration_secs`
    /// (`None` = until the stream ends).
    Pin {
        message: String,
        duration_secs: Option<u32>,
    },
    Clear,
    /// `Some(secs)` on (Twitch allows 3–120), `None` off.
    Slow(Option<u32>),
    /// `Some(minutes)` of minimum follow age on (0 = any follower), `None` off.
    Followers(Option<u32>),
    SubOnly(bool),
    EmoteOnly(bool),
    UniqueChat(bool),
    Role {
        role: Role,
        grant: bool,
        user: String,
    },
    Shoutout {
        user: String,
    },
    Raid {
        target: String,
    },
    Unraid,
}

/// The two native Twitch emote sets [`Controller::fetch_twitch_emotes`]
/// returns: `personal` is everything the logged-in user can use (autocomplete
/// draws only from these), `channel` is the viewed channel's remaining
/// (locked) emotes, shown in the picker only. Serde so the last fetch can be
/// disk-cached for a warm start (see [`crate::emote_cache`]).
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TwitchEmotes {
    #[serde(default)]
    pub personal: Vec<bks_core::Emote>,
    #[serde(default)]
    pub channel: Vec<bks_core::Emote>,
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
        // A Kick-only tab has nowhere else to send, so it starts (and stays)
        // targeted at Kick; anything else defaults to Twitch.
        let target = if twitch_channel.is_empty() && !kick_channel.is_empty() {
            SendTarget::Kick
        } else {
            SendTarget::default()
        };
        Self {
            state: Arc::new(Mutex::new(State::default())),
            target: Arc::new(std::sync::Mutex::new(target)),
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

    /// The tokio runtime handle, for spawning app-global background work that
    /// isn't tied to this tab's channel/session (currently the link-preview
    /// fetch, whose cache is process-wide).
    pub fn runtime(&self) -> Handle {
        self.rt.clone()
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
            this.refresh_kick_mod_status().await;
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
                // Compare the account, not just the flag, so a re-login as a
                // different Kick user re-resolves too.
                if now.kick != prev.kick {
                    this.refresh_kick_mod_status().await;
                }
                prev = now;
            }
        });
    }

    /// Resolves whether the logged-in Kick account moderates this tab's Kick
    /// channel and reports it as a `ModStatus` event (Kick's equivalent of the
    /// Twitch USERSTATE flag — it gates the usercard mod panel + mod-button
    /// strip via `ChannelModel::can_moderate`). Logged out = not a mod; the
    /// broadcaster is a mod by definition (their own usercard doesn't carry the
    /// flag); anyone else is answered by the anonymous per-channel usercard
    /// lookup. A failed lookup keeps the previous state — a transient
    /// Cloudflare hiccup shouldn't strip a real mod's buttons — and mid-session
    /// grants/removals are corrected from our own messages' badges
    /// (`sync_kick_mod_from_message`).
    async fn refresh_kick_mod_status(&self) {
        if self.kick_channel.is_empty() {
            return;
        }
        let (is_mod, is_broadcaster) = match self.session.login_state().kick {
            None => (false, false),
            Some(_) if self.kick_is_broadcaster() => (true, true),
            Some(login) => match bks_kick::fetch_user_info(&self.kick_channel, &login).await {
                Ok(info) => (info.is_moderator, false),
                Err(err) => {
                    tracing::warn!(
                        "kick mod-status lookup for {} in {} failed: {err:#}",
                        login,
                        self.kick_channel
                    );
                    return;
                }
            },
        };
        let _ = self
            .events
            .send(ChatEvent::ModStatus {
                platform: bks_core::Platform::Kick,
                is_mod,
                is_broadcaster,
            })
            .await;
    }

    /// Keeps Kick mod status live between logins: every Kick message carries its
    /// author's badges, so the logged-in account's own lines (Kick echoes our
    /// sends back over Pusher) answer "do I mod here?" more freshly than the
    /// login-time lookup — being modded or demodded mid-session corrects itself
    /// the moment the user chats. Historical (backlog) messages are skipped:
    /// their badges are as old as the message. The store dedupes, so re-asserts
    /// with the same value never re-measure the log.
    pub fn sync_kick_mod_from_message(&self, msg: &bks_core::Message) {
        if msg.historical {
            return;
        }
        let Some(login) = self.session.login_state().kick else {
            return;
        };
        if bks_kick::slugify(&login) != bks_kick::slugify(&msg.author.login) {
            return;
        }
        let is_broadcaster = msg.author.badges.iter().any(|b| b.id == "broadcaster");
        let is_mod = is_broadcaster || msg.author.badges.iter().any(|b| b.id == "moderator");
        let _ = self.events.try_send(ChatEvent::ModStatus {
            platform: bks_core::Platform::Kick,
            is_mod,
            is_broadcaster,
        });
    }

    /// On Kick logout, a tab targeting Kick/Both can no longer send there, so
    /// fall back to Twitch — unless the tab has no Twitch channel, where Kick
    /// stays the target so a send gets the "log into Kick first" hint.
    fn reset_target_off_kick(&self) {
        if !self.has_twitch() {
            return;
        }
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

    /// Whether the user is logged into Twitch (the composer's placeholder).
    pub fn twitch_logged_in(&self) -> bool {
        self.session.login_state().twitch()
    }

    /// Whether the logged-in Kick account owns this tab's Kick channel
    /// (compared as slugs — the login is a display-style username). Twitch's
    /// equivalent is `ChannelModel.twitch_broadcaster` (USERSTATE badge).
    pub fn kick_is_broadcaster(&self) -> bool {
        !self.kick_channel.is_empty()
            && self.session.login_state().kick.is_some_and(|login| {
                bks_kick::slugify(&login) == bks_kick::slugify(&self.kick_channel)
            })
    }

    /// Whether this tab has a Kick channel set (the toggle is only useful then).
    pub fn has_kick(&self) -> bool {
        !self.kick_channel.is_empty()
    }

    /// Whether this tab has a Twitch channel set.
    pub fn has_twitch(&self) -> bool {
        !self.twitch_channel.is_empty()
    }

    /// The tab's Twitch channel login (empty if none is set).
    pub fn twitch_channel(&self) -> &str {
        &self.twitch_channel
    }

    /// Cycles the send target Twitch → Kick → Both (only meaningful when logged
    /// into Kick and this tab has both channels — a single-platform tab has
    /// nothing to switch to).
    pub fn cycle_send_target(&self) {
        if !self.kick_logged_in() || !self.has_kick() || !self.has_twitch() {
            return;
        }
        let mut target = self.target.lock().unwrap();
        *target = match *target {
            SendTarget::Twitch => SendTarget::Kick,
            SendTarget::Kick => SendTarget::Both,
            SendTarget::Both => SendTarget::Twitch,
        };
        tracing::info!("send target: {}", target.label());
    }

    /// Switches the send target to a specific platform (used by right-click-to-tag:
    /// right-clicking a chatter targets their platform so the tag lands in the right
    /// chat). A no-op when the tab lacks that platform, or when it's Kick and the
    /// user isn't logged into Kick (there's nothing to send to). Logs the
    /// "send target: X" line only on a real change; returns whether it changed.
    pub fn set_send_target(&self, platform: bks_core::Platform) -> bool {
        let want = match platform {
            bks_core::Platform::Twitch if self.has_twitch() => SendTarget::Twitch,
            bks_core::Platform::Kick if self.has_kick() && self.kick_logged_in() => SendTarget::Kick,
            _ => return false,
        };
        let mut target = self.target.lock().unwrap();
        if *target == want {
            return false;
        }
        *target = want;
        tracing::info!("send target: {}", want.label());
        true
    }

    /// Reports a user-facing error in this tab's feed (a copyable error row).
    pub(crate) fn notice(&self, msg: impl Into<String>) {
        let _ = self.events.try_send(ChatEvent::Error(msg.into()));
    }

    /// Posts a muted confirmation row (a successful command with no other
    /// visible effect — warn/shoutout/raid). Errors go through [`Self::notice`].
    fn confirm(&self, msg: impl Into<String>) {
        let _ = self.events.try_send(ChatEvent::Notice(msg.into()));
    }

    /// The Twitch actions client, posting the "log in first" hint when logged
    /// out — the shared guard of every authed Twitch method here.
    async fn twitch_actions_or_hint(&self) -> Option<Arc<TwitchActions>> {
        let actions = self.session.twitch_actions().await;
        if actions.is_none() {
            self.notice("log into Twitch first (Settings → Account)");
        }
        actions
    }

    /// The Kick actions client, posting the "log in first" hint when logged out.
    async fn kick_actions_or_hint(&self) -> Option<Arc<KickActions>> {
        let actions = self.session.kick_actions().await;
        if actions.is_none() {
            self.notice("log into Kick first (Settings → Account)");
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
        self.spawn_twitch_mod(ModAction::Timeout {
            user,
            secs,
            reason: None,
        });
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
            if let Err(err) = apply_role(&actions, &this.twitch_channel, role, grant, &user).await
            {
                this.notice(format!("{err:#}"));
            }
        });
    }

    /// Warns `user` on Twitch with `reason` (usercard Warn button — acts on the
    /// Twitch chatter regardless of the tab's send target, like the ban/timeout
    /// buttons). The chatter must acknowledge the warning before chatting again.
    pub fn warn_twitch(&self, user: String, reason: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.twitch_actions_or_hint().await else {
                return;
            };
            if !this.require_twitch_channel() {
                return;
            }
            match actions.warn(&this.twitch_channel, &user, &reason).await {
                Ok(()) => this.confirm(format!("warned {user}")),
                Err(err) => this.notice(format!("{err:#}")),
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
                    "log into Twitch (Settings → Account) to fetch the viewer list — \
                     Twitch only shows it to the broadcaster and moderators"
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

    /// Fetches the logged-in user's native Twitch emotes for the picker +
    /// autocomplete, delivering both sets over `reply`: the emotes the user can
    /// actually use (sub/follower/global — autocomplete draws only from these)
    /// and the viewed channel's own set with the usable ones removed (picker
    /// display only — a locked sub emote shows there like Twitch web, but must
    /// never autocomplete into a message that would send as plain text).
    /// Sends empty sets when not logged into Twitch.
    ///
    /// May send **twice**: last session's disk-cached sets first (instant warm
    /// start at launch), then the fresh fetch, which also re-persists the
    /// cache. The receiver just applies whatever arrives, in order.
    pub fn fetch_twitch_emotes(&self, reply: smol::channel::Sender<TwitchEmotes>) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.session.twitch_actions().await else {
                let _ = reply.send(TwitchEmotes::default()).await;
                return;
            };
            let channel = this.twitch_channel.clone();
            let had_cache = match emote_cache::load(actions.own_user_id(), &channel) {
                Some(cached) => reply.send(cached).await.is_ok(),
                None => false,
            };
            // Fetched concurrently; a failure of either just yields fewer
            // emotes. The channel makes its follower emotes part of the
            // usable set too.
            let channel = (!channel.is_empty()).then_some(channel);
            let (personal, channel_emotes) = actions.native_emotes(channel.as_deref()).await;
            let personal = match personal {
                Ok(p) => p,
                Err(e) => {
                    // A 429 is transient rate-limiting (many tabs fetching at
                    // once), not a scope problem — say so rather than sending the
                    // user to re-login for nothing. Anything else is most likely
                    // an old token without `user:read:emotes`, so cross-channel
                    // sub emotes stay out of the picker + autocomplete until the
                    // next /login.
                    let rate_limited = e
                        .downcast_ref::<reqwest::Error>()
                        .and_then(|re| re.status())
                        == Some(reqwest::StatusCode::TOO_MANY_REQUESTS);
                    if rate_limited {
                        tracing::warn!(
                            "fetching personal Twitch emotes was rate-limited by \
                             Twitch (429); reopen the picker to retry"
                        );
                    } else {
                        tracing::warn!(
                            "fetching personal Twitch emotes failed ({e:#}); \
                             log out and back in if your token predates the \
                             user:read:emotes scope"
                        );
                    }
                    if had_cache {
                        // Keep showing the cached sets rather than clobbering
                        // them with an empty fetch.
                        return;
                    }
                    Vec::new()
                }
            };
            let usable: std::collections::HashSet<&str> =
                personal.iter().map(|e| e.name.as_str()).collect();
            let channel_ok = channel_emotes.is_ok();
            let channel_set: Vec<bks_core::Emote> = channel_emotes
                .unwrap_or_default()
                .into_iter()
                .filter(|e| !usable.contains(e.name.as_str()))
                .collect();
            let emotes = TwitchEmotes {
                personal,
                channel: channel_set,
            };
            // A failed channel listing keeps the previous cache entry (the
            // fresh send below still updates the screen with what we got).
            if channel_ok {
                emote_cache::save(
                    actions.own_user_id(),
                    &this.twitch_channel,
                    &emotes,
                );
            }
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
        self.spawn_kick_mod(ModAction::Timeout {
            user,
            secs,
            reason: None,
        });
    }

    /// Lifts a Kick ban/timeout on `user` (usercard Unban button).
    pub fn unban_kick(&self, user: String) {
        self.spawn_kick_mod(ModAction::Unban { user });
    }

    /// Default pin duration (seconds) — the web clients' usual 20 minutes
    /// (Twitch allows 30–1800s, Kick sends 1200). The pin dialog's chips and
    /// `/pin`'s leading duration override it.
    pub(crate) const PIN_DURATION_SECS: u32 = 1200;

    /// Pins the Twitch message `message_id` in this tab's channel for
    /// `duration_secs` (`None` = until the stream ends) — the pin dialog's
    /// choice. Needs to moderate the channel; a 403 surfaces as the error row.
    pub fn pin_twitch(&self, message_id: String, duration_secs: Option<u32>) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.twitch_actions_or_hint().await else {
                return;
            };
            if !this.require_twitch_channel() {
                return;
            }
            if let Err(err) = actions
                .pin_message(&this.twitch_channel, &message_id, duration_secs)
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

    /// The notice for pin/unpin attempts on Kick: those only exist on
    /// kick.com's site API, which rejects public-API OAuth tokens (it wants the
    /// web client's session token) — disabled until Kick adds public endpoints
    /// (like it eventually did for delete-message).
    pub(crate) const KICK_UNSUPPORTED: &'static str =
        "Kick's API doesn't let third-party apps do this yet — disabled until Kick adds it";

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
            self.handle_command(rest, self.send_target());
        } else if let Some(reply) = reply {
            self.send_reply(line.to_string(), reply);
        } else {
            self.send(line.to_string(), self.send_target());
        }
    }

    /// Like [`handle_input`](Self::handle_input) but targeted at an explicit
    /// platform instead of the send-target toggle — the per-message mod buttons
    /// act on the *row's* platform (a button on a Kick message moderates Kick
    /// even while the composer sends to Twitch). Plain text is sent to that
    /// platform's chat.
    pub fn handle_input_at(&self, line: &str, platform: bks_core::Platform) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let target = match platform {
            bks_core::Platform::Twitch => SendTarget::Twitch,
            bks_core::Platform::Kick => SendTarget::Kick,
            _ => return self.notice("mod actions aren't supported on this platform"),
        };
        if let Some(rest) = line.strip_prefix('/') {
            self.handle_command(rest, target);
        } else {
            self.send(line.to_string(), target);
        }
    }

    fn handle_command(&self, rest: &str, target: SendTarget) {
        let mut parts = rest.split_whitespace();
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let args: Vec<&str> = parts.collect();
        // Commands act on ONE chat; in Both mode there's no unambiguous target.
        if target == SendTarget::Both {
            return self.notice(
                "commands don't work while sending to both platforms — \
                 switch the send target to one",
            );
        }
        match cmd.as_str() {
            "ban" => match args.split_first() {
                Some((user, reason)) => self.moderate(target, ModAction::Ban {
                    user: user_arg(user),
                    reason: join(reason),
                }),
                None => self.notice("usage: /ban <user> [reason]"),
            },
            // Timeout is in seconds here; converted to minutes for Kick downstream.
            "timeout" => match args.as_slice() {
                [user, duration, reason @ ..] => match bks_core::parse_duration(duration)
                    .and_then(|secs| u32::try_from(secs).ok())
                {
                    Some(secs) => self.moderate(target, ModAction::Timeout {
                        user: user_arg(user),
                        secs,
                        reason: join(reason),
                    }),
                    None => self.notice(
                        "usage: /timeout <user> <duration — 600, 30m, 1h, 3d, 1w> [reason]",
                    ),
                },
                _ => self.notice("usage: /timeout <user> <duration — 600, 30m, 1h, 3d, 1w> [reason]"),
            },
            "unban" | "untimeout" => match args.first() {
                Some(user) => self.moderate(target, ModAction::Unban {
                    user: user_arg(user),
                }),
                None => self.notice(format!("usage: /{cmd} <user>")),
            },
            "delete" => match args.first() {
                Some(id) => self.moderate(target, ModAction::Delete {
                    message_id: id.to_string(),
                }),
                None => self.notice("usage: /delete <message-id>"),
            },
            "me" => match join(&args) {
                Some(text) => self.send_me(text, target),
                None => self.notice("usage: /me <message>"),
            },
            "announce" | "announceblue" | "announcegreen" | "announceorange"
            | "announcepurple" => match join(&args) {
                Some(message) => {
                    // The suffix is the Helix color name ("" = channel accent).
                    let color = match cmd.strip_prefix("announce").unwrap_or("") {
                        "" => None,
                        c => Some(match c {
                            "blue" => "blue",
                            "green" => "green",
                            "orange" => "orange",
                            _ => "purple",
                        }),
                    };
                    self.twitch_cmd(target, &cmd, TwitchCmd::Announce { message, color });
                }
                None => self.notice(format!("usage: /{cmd} <message>")),
            },
            "warn" => match args.split_first() {
                Some((user, reason)) if !reason.is_empty() => self.twitch_cmd(
                    target,
                    &cmd,
                    TwitchCmd::Warn {
                        user: user_arg(user),
                        reason: reason.join(" "),
                    },
                ),
                _ => self.notice("usage: /warn <user> <reason>"),
            },
            "pin" => {
                // Optional leading duration ("/pin 10m message"); a bare
                // number is MINUTES ("/pin 20 hi" = 20m — pins are minutes-
                // scale, unlike /timeout's seconds); without one the pin
                // stays until the stream ends. A first word that doesn't
                // parse as a duration is just part of the message; a leading
                // "--" (or "-") skips duration parsing entirely, so a message
                // that *starts* with a number can still pin unlimited
                // ("/pin -- 20 chickens"). Only the first token is ever read
                // as a duration — a timed pin's message is never at risk.
                let (duration_secs, message_args) = match args.split_first() {
                    Some((first, rest)) if *first == "--" || *first == "-" => (None, rest),
                    Some((first, rest)) if !rest.is_empty() => {
                        let parsed = if first.bytes().all(|b| b.is_ascii_digit()) {
                            first.parse::<u64>().ok().and_then(|mins| mins.checked_mul(60))
                        } else {
                            bks_core::parse_duration(first)
                        };
                        match parsed.and_then(|secs| u32::try_from(secs).ok()) {
                            Some(secs) if (30..=1800).contains(&secs) => (Some(secs), rest),
                            Some(_) => {
                                return self
                                    .notice("pin duration must be between 30s and 30m");
                            }
                            None => (None, args.as_slice()),
                        }
                    }
                    _ => (None, args.as_slice()),
                };
                match join(message_args) {
                    Some(message) => self.twitch_cmd(
                        target,
                        &cmd,
                        TwitchCmd::Pin {
                            message,
                            duration_secs,
                        },
                    ),
                    None => self.notice(
                        "usage: /pin [duration] <message> — sends your message and pins it \
                         (no duration = until the stream ends; start with -- to pin a \
                         message that begins with a number)",
                    ),
                }
            }
            "clear" => self.twitch_cmd(target, &cmd, TwitchCmd::Clear),
            "slow" => match args.first() {
                // Twitch web's default wait time.
                None => self.twitch_cmd(target, &cmd, TwitchCmd::Slow(Some(30))),
                // Pre-check Helix's 3–120s range so an out-of-range value gets
                // the usage hint instead of a raw Helix 400 error row.
                Some(arg) => match bks_core::parse_duration(arg)
                    .and_then(|secs| u32::try_from(secs).ok())
                {
                    Some(secs) if (3..=120).contains(&secs) => {
                        self.twitch_cmd(target, &cmd, TwitchCmd::Slow(Some(secs)))
                    }
                    _ => self.notice("usage: /slow [seconds — 3 to 120]"),
                },
            },
            "slowoff" => self.twitch_cmd(target, &cmd, TwitchCmd::Slow(None)),
            "followers" => match args.first() {
                // No minimum follow age — any follower can chat.
                None => self.twitch_cmd(target, &cmd, TwitchCmd::Followers(Some(0))),
                // A bare number is minutes (Helix's unit, like twitch.tv's
                // command — and unlike /timeout's seconds); 0 = any follower.
                Some(arg) => match arg
                    .parse::<u32>()
                    .ok()
                    .or_else(|| {
                        bks_core::parse_duration(arg)
                            .map(|secs| u32::try_from(secs.div_ceil(60)).unwrap_or(u32::MAX))
                    }) {
                    Some(minutes) => self.twitch_cmd(target, &cmd, TwitchCmd::Followers(Some(minutes))),
                    None => {
                        self.notice("usage: /followers [duration — 10 = 10m, 1h, 30d; 0 = any follower]")
                    }
                },
            },
            "followersoff" => self.twitch_cmd(target, &cmd, TwitchCmd::Followers(None)),
            "subscribers" => self.twitch_cmd(target, &cmd, TwitchCmd::SubOnly(true)),
            "subscribersoff" => self.twitch_cmd(target, &cmd, TwitchCmd::SubOnly(false)),
            "emoteonly" => self.twitch_cmd(target, &cmd, TwitchCmd::EmoteOnly(true)),
            "emoteonlyoff" => self.twitch_cmd(target, &cmd, TwitchCmd::EmoteOnly(false)),
            "uniquechat" => self.twitch_cmd(target, &cmd, TwitchCmd::UniqueChat(true)),
            "uniquechatoff" => self.twitch_cmd(target, &cmd, TwitchCmd::UniqueChat(false)),
            "mod" | "unmod" | "vip" | "unvip" => match args.first() {
                Some(user) => self.twitch_cmd(
                    target,
                    &cmd,
                    TwitchCmd::Role {
                        role: if cmd.ends_with("vip") {
                            Role::Vip
                        } else {
                            Role::Moderator
                        },
                        grant: !cmd.starts_with("un"),
                        user: user_arg(user),
                    },
                ),
                None => self.notice(format!("usage: /{cmd} <user>")),
            },
            "shoutout" => match args.first() {
                Some(user) => self.twitch_cmd(
                    target,
                    &cmd,
                    TwitchCmd::Shoutout {
                        user: user_arg(user),
                    },
                ),
                None => self.notice("usage: /shoutout <channel>"),
            },
            "raid" => match args.first() {
                Some(raid_target) => self.twitch_cmd(
                    target,
                    &cmd,
                    TwitchCmd::Raid {
                        target: user_arg(raid_target),
                    },
                ),
                None => self.notice("usage: /raid <channel>"),
            },
            "unraid" => self.twitch_cmd(target, &cmd, TwitchCmd::Unraid),
            other => self.notice(format!("unknown command: /{other}")),
        }
    }

    /// Runs a Twitch-only command against this tab's Twitch channel — refused
    /// when the target is Kick (the Both case is gated in
    /// [`Self::handle_command`]).
    fn twitch_cmd(&self, target: SendTarget, name: &str, cmd: TwitchCmd) {
        if target == SendTarget::Kick {
            return self.notice(format!(
                "/{name} is Twitch-only — switch the send target to Twitch"
            ));
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(actions) = this.twitch_actions_or_hint().await else {
                return;
            };
            if !this.require_twitch_channel() {
                return;
            }
            let ch = &this.twitch_channel;
            // Each command's Helix call, plus the confirmation for the ones
            // whose success is otherwise invisible in chat.
            let (result, done) = match cmd {
                TwitchCmd::Announce { message, color } => {
                    (actions.announce(ch, &message, color).await, None)
                }
                TwitchCmd::Warn { user, reason } => (
                    actions.warn(ch, &user, &reason).await,
                    Some(format!("warned {user}")),
                ),
                TwitchCmd::Pin {
                    message,
                    duration_secs,
                } => (actions.send_and_pin(ch, &message, duration_secs).await, None),
                TwitchCmd::Clear => (actions.clear_chat(ch).await, None),
                TwitchCmd::Slow(secs) => (actions.set_slow_mode(ch, secs).await, None),
                TwitchCmd::Followers(minutes) => {
                    (actions.set_follower_mode(ch, minutes).await, None)
                }
                TwitchCmd::SubOnly(on) => (actions.set_sub_only(ch, on).await, None),
                TwitchCmd::EmoteOnly(on) => (actions.set_emote_only(ch, on).await, None),
                TwitchCmd::UniqueChat(on) => (actions.set_unique_chat(ch, on).await, None),
                TwitchCmd::Role { role, grant, user } => {
                    let result = apply_role(&actions, ch, role, grant, &user).await;
                    let verb = match (role, grant) {
                        (Role::Moderator, true) => "modded",
                        (Role::Moderator, false) => "unmodded",
                        (Role::Vip, true) => "granted VIP to",
                        (Role::Vip, false) => "removed VIP from",
                    };
                    (result, Some(format!("{verb} {user}")))
                }
                TwitchCmd::Shoutout { user } => (
                    actions.shoutout(ch, &user).await,
                    Some(format!("shouted out {user}")),
                ),
                TwitchCmd::Raid { target } => (
                    actions.raid(ch, &target).await,
                    Some(format!("raiding {target}")),
                ),
                TwitchCmd::Unraid => (actions.unraid(ch).await, Some("raid cancelled".into())),
            };
            match result {
                Ok(()) => {
                    if let Some(done) = done {
                        this.confirm(done);
                    }
                }
                Err(err) => this.notice(format!("{err:#}")),
            }
        });
    }

    /// Sends `/me text` as an action message on Twitch (the one slash command
    /// Twitch IRC still interprets inside a PRIVMSG). Kick has no equivalent.
    fn send_me(&self, text: String, target: SendTarget) {
        if target == SendTarget::Kick {
            return self.notice("/me is Twitch-only — switch the send target to Twitch");
        }
        let this = self.clone();
        self.rt.spawn(async move {
            if !this.require_twitch_channel() {
                return;
            }
            let src = this.state.lock().await.twitch.clone();
            match src {
                Some(src) => {
                    if let Err(err) = src
                        .send(&this.twitch_channel, &format!("/me {text}"), None)
                        .await
                    {
                        this.notice(format!("twitch: {err:#}"));
                    }
                }
                None => this.notice("log into Twitch first (Settings → Account)"),
            }
        });
    }

    fn send(&self, text: String, target: SendTarget) {
        let this = self.clone();
        self.rt.spawn(async move {
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
                    None => this.notice("not logged into Kick (Settings → Account)"),
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
                        None => this.notice("log into Twitch to reply (Settings → Account)"),
                    }
                }
                bks_core::Platform::Kick => match this.session.kick_actions().await {
                    Some(actions) => {
                        this.send_kick(&actions, &text, Some(&reply.message_id))
                            .await
                    }
                    None => this.notice("log into Kick to reply (Settings → Account)"),
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
    fn moderate(&self, target: SendTarget, action: ModAction) {
        let this = self.clone();
        self.rt.spawn(async move {
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
        let target_user = match &action {
            ModAction::Ban { user, .. }
            | ModAction::Timeout { user, .. }
            | ModAction::Unban { user } => user.clone(),
            ModAction::Delete { .. } => String::new(),
        };
        let result = match action {
            ModAction::Ban { user, reason } => actions.ban(ch, &user, reason.as_deref()).await,
            ModAction::Timeout { user, secs, reason } => {
                actions.timeout(ch, &user, secs, reason.as_deref()).await
            }
            ModAction::Unban { user } => actions.unban(ch, &user).await,
            ModAction::Delete { message_id } => actions.delete_message(ch, &message_id).await,
        };
        if let Err(err) = result {
            self.report_twitch_mod_error(&target_user, &err);
        }
    }

    /// Reports a failed Twitch moderation action. Some Helix "errors" are really
    /// just information the user should see (e.g. unbanning someone who isn't
    /// banned) — those render as a muted [`confirm`](Self::confirm) notice rather
    /// than a red error row. Add a `(needle, friendly)` entry here to reclassify
    /// another response; `{user}` in the friendly text is replaced with the
    /// target. Everything else falls through to the copyable error row.
    fn report_twitch_mod_error(&self, user: &str, err: &anyhow::Error) {
        // Matched against the Helix `message` field carried in the error string.
        const INFO: &[(&str, &str)] = &[(
            "user in the user_id query parameter is not banned",
            "{user} is not banned",
        )];
        let text = format!("{err:#}");
        for (needle, friendly) in INFO {
            if text.contains(needle) {
                self.confirm(friendly.replace("{user}", user));
                return;
            }
        }
        self.notice(text);
    }

    async fn moderate_kick(&self, actions: &KickActions, action: ModAction) {
        if !self.require_kick_channel() {
            return;
        }
        // Delete keys on the message id alone (public API
        // `DELETE /chat/{message_id}`), not on a chatter id like the ban family.
        if let ModAction::Delete { message_id } = &action {
            if let Err(err) = actions.delete_message(message_id).await {
                self.report_kick_error(&err);
            }
            return;
        }
        // Ban/timeout/unban all need numeric ids resolved from chatters we've
        // seen (Kick's API can't look up a user by name).
        let user = match &action {
            ModAction::Ban { user, .. }
            | ModAction::Timeout { user, .. }
            | ModAction::Unban { user } => user.clone(),
            ModAction::Delete { .. } => unreachable!("handled above"),
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
            ModAction::Timeout { secs, reason, .. } => {
                let minutes = secs.div_ceil(60).max(1);
                actions
                    .ban(broadcaster_id, target_id, Some(minutes), reason.as_deref())
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

/// The Helix call for one role grant/revoke — shared by the usercard buttons
/// ([`Controller::set_role_twitch`]) and the `/mod` command family so the two
/// dispatch tables can't drift.
async fn apply_role(
    actions: &TwitchActions,
    channel: &str,
    role: Role,
    grant: bool,
    user: &str,
) -> anyhow::Result<()> {
    match (role, grant) {
        (Role::Moderator, true) => actions.add_moderator(channel, user).await,
        (Role::Moderator, false) => actions.remove_moderator(channel, user).await,
        (Role::Vip, true) => actions.add_vip(channel, user).await,
        (Role::Vip, false) => actions.remove_vip(channel, user).await,
    }
}

fn join(parts: &[&str]) -> Option<String> {
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// A typed user/channel argument, cleaned: the mention autocomplete inserts
/// `@name` (and typing the `@` by hand is habit), but Helix login lookups
/// reject it and the Kick `login → id` map is keyed on bare names.
fn user_arg(arg: &str) -> String {
    arg.trim_start_matches('@').to_string()
}
