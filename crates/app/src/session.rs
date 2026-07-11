//! App-wide login, the single source of truth for auth — and observable.
//!
//! There is one Twitch login and one Kick login for the whole app. Login state is
//! owned *only* here and mutated *only* through these methods, callable from
//! anywhere (a tab command today, a settings screen or a token-refresh task
//! later). Tabs never own or assume auth; they **observe** it.
//!
//! A `watch` carrying a [`LoginState`] snapshot fans changes out to every tab,
//! regardless of what caused them (user logout, token expiry, failed refresh,
//! login from settings): tabs reconcile their connection + send target against it
//! (see `controller.rs`). Login/logout *status* is logged (not shown in chat);
//! only genuine errors for a login *attempt* go to the issuing tab's feed.
//!
//! Being logged out of a platform is a normal state, not an error: a tab simply
//! falls back to an anonymous read connection (chat still flows, sending is
//! disabled) until login returns.

use std::sync::Arc;

use bks_kick::KickActions;
use bks_platform::ChatEvent;
use bks_twitch::{EventsubAuth, TwitchActions, TwitchAuth, TwitchSource};
use tokio::runtime::Handle;
use tokio::sync::{watch, Mutex};

type Sink = smol::channel::Sender<ChatEvent>;

/// A snapshot of what's logged in, broadcast to every tab on each change. Carries
/// only what subscribers need to reconcile + display (the account names, not the
/// secrets behind the actions). `Some(name)` means logged in as that account.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LoginState {
    pub twitch: Option<String>,
    pub kick: Option<String>,
}

impl LoginState {
    /// Whether Twitch is logged in (for subscribers that only need the flag).
    pub fn twitch(&self) -> bool {
        self.twitch.is_some()
    }

    /// Whether Kick is logged in.
    pub fn kick(&self) -> bool {
        self.kick.is_some()
    }
}

/// The mutable login state behind the session mutex. The `Arc` action handles and
/// auth are what tabs pull at send/connect time; the booleans mirror into the
/// broadcast snapshot.
#[derive(Default)]
struct State {
    twitch_actions: Option<Arc<TwitchActions>>,
    twitch_auth: Option<TwitchAuth>,
    /// Credentials for the EventSub moderator feed (token + granted scopes) —
    /// what a tab passes to `run_twitch` so it can subscribe as this user.
    twitch_eventsub: Option<EventsubAuth>,
    kick_actions: Option<Arc<KickActions>>,
    kick_login: Option<String>,
}

impl State {
    fn snapshot(&self) -> LoginState {
        LoginState {
            twitch: self.twitch_auth.as_ref().map(|a| a.login.clone()),
            kick: self.kick_login.clone(),
        }
    }
}

/// The shared, app-wide login. Clone is cheap (`Arc`/`Handle`/`watch`).
#[derive(Clone)]
pub struct Session {
    state: Arc<Mutex<State>>,
    rt: Handle,
    /// Broadcasts a [`LoginState`] snapshot on every auth change.
    changes: watch::Sender<LoginState>,
    /// Built-in/overridable Twitch app id (Twitch needs no secret).
    twitch_client_id: String,
}

impl Session {
    /// Builds the session, synchronously loading any saved logins so tabs connect
    /// authenticated from their very first join (no anonymous→authed reconnect
    /// race at startup).
    pub fn new(rt: Handle, twitch_client_id: String) -> Self {
        let mut state = State::default();
        if let Ok(Some(creds)) = bks_auth::twitch::load() {
            state.twitch_auth = Some(TwitchAuth {
                login: creds.login.clone(),
                oauth_pass: creds.irc_pass(),
            });
            state.twitch_eventsub = Some(EventsubAuth {
                client_id: twitch_client_id.clone(),
                token: creds.access_token.clone(),
                user_id: creds.user_id.clone(),
                scopes: creds.scopes.clone(),
            });
            state.twitch_actions = Some(Arc::new(TwitchActions::new(
                twitch_client_id.clone(),
                creds.access_token,
                creds.user_id,
            )));
        }
        if let Ok(Some(creds)) = bks_auth::kick::load() {
            // The actions refresh their own token on 401; the persist callback is
            // attached just below (once `Session` exists to hold it).
            state.kick_actions = Some(Arc::new(kick_actions(&creds)));
            state.kick_login = Some(creds.username);
        }

        let (changes, _) = watch::channel(state.snapshot());
        let session = Self {
            state: Arc::new(Mutex::new(state)),
            rt,
            changes,
            twitch_client_id,
        };
        session.attach_kick_refresh();
        session
    }

    /// Wires the Kick actions' refresh callback (if any are loaded) so a token
    /// refreshed on a 401 is persisted to disk — otherwise the rotated refresh
    /// token would be lost and the next launch would start with a dead token.
    fn attach_kick_refresh(&self) {
        let this = self.clone();
        self.rt.spawn(async move {
            let actions = this.state.lock().await.kick_actions.clone();
            if let Some(actions) = actions {
                actions.set_on_refreshed(persist_refreshed_kick()).await;
            }
        });
    }

    /// A receiver tabs use to observe login changes. The current value is the
    /// latest snapshot, so a tab reconciles correctly even if it subscribes after
    /// a change.
    pub fn subscribe(&self) -> watch::Receiver<LoginState> {
        self.changes.subscribe()
    }

    /// The current Twitch auth, if logged in — a tab uses it to join authed.
    pub async fn twitch_auth(&self) -> Option<TwitchAuth> {
        self.state.lock().await.twitch_auth.clone()
    }

    pub async fn twitch_actions(&self) -> Option<Arc<TwitchActions>> {
        self.state.lock().await.twitch_actions.clone()
    }

    /// Credentials for the EventSub moderator feed, if logged into Twitch. The
    /// scopes let the feed decide locally what it can subscribe to (an old token
    /// from before the feed's scopes just leaves it off).
    pub async fn twitch_eventsub(&self) -> Option<EventsubAuth> {
        self.state.lock().await.twitch_eventsub.clone()
    }

    pub async fn kick_actions(&self) -> Option<Arc<KickActions>> {
        self.state.lock().await.kick_actions.clone()
    }

    pub fn kick_logged_in(&self) -> bool {
        self.changes.borrow().kick()
    }

    /// The current login snapshot, read synchronously (no await) so the GPUI side
    /// can render account status. Reflects the latest broadcast state.
    pub fn login_state(&self) -> LoginState {
        self.changes.borrow().clone()
    }

    /// Runs Twitch OAuth in the browser, saves, and applies the login. `events`
    /// is where progress/error notices for *this attempt* go (the issuing feed);
    /// the resulting state change is broadcast to all tabs.
    pub fn twitch_login(&self, events: Sink) {
        let this = self.clone();
        tracing::info!("opening browser for Twitch login");
        self.rt.spawn(async move {
            let creds = match bks_auth::twitch::login(&this.twitch_client_id).await {
                Ok(c) => c,
                Err(err) => {
                    let _ =
                        events.try_send(ChatEvent::Error(format!("twitch login failed: {err:#}")));
                    return;
                }
            };
            if let Err(err) = bks_auth::twitch::save(&creds) {
                let _ = events.try_send(ChatEvent::Error(format!(
                    "could not save Twitch login: {err:#}"
                )));
            }
            this.apply_twitch(creds).await;
        });
    }

    pub fn twitch_logout(&self) {
        let this = self.clone();
        let _ = bks_auth::twitch::clear();
        self.rt.spawn(async move {
            {
                let mut s = this.state.lock().await;
                s.twitch_actions = None;
                s.twitch_auth = None;
                s.twitch_eventsub = None;
            }
            this.broadcast("logged out of Twitch").await;
        });
    }

    pub fn kick_login(&self, events: Sink) {
        let broker = bks_auth::kick::broker_url();
        let this = self.clone();
        tracing::info!("opening browser for Kick login");
        self.rt.spawn(async move {
            let creds = match bks_auth::kick::login(&broker).await {
                Ok(c) => c,
                Err(err) => {
                    let _ =
                        events.try_send(ChatEvent::Error(format!("kick login failed: {err:#}")));
                    return;
                }
            };
            if let Err(err) = bks_auth::kick::save(&creds) {
                let _ = events.try_send(ChatEvent::Error(format!(
                    "could not save Kick login: {err:#}"
                )));
            }
            this.apply_kick(creds).await;
        });
    }

    pub fn kick_logout(&self) {
        let this = self.clone();
        let _ = bks_auth::kick::clear();
        self.rt.spawn(async move {
            {
                let mut s = this.state.lock().await;
                s.kick_actions = None;
                s.kick_login = None;
            }
            this.broadcast("logged out of Kick").await;
        });
    }

    async fn apply_twitch(&self, creds: bks_auth::twitch::Credentials) {
        let auth = TwitchAuth {
            login: creds.login.clone(),
            oauth_pass: creds.irc_pass(),
        };
        let eventsub = EventsubAuth {
            client_id: self.twitch_client_id.clone(),
            token: creds.access_token.clone(),
            user_id: creds.user_id.clone(),
            scopes: creds.scopes.clone(),
        };
        let actions = Arc::new(TwitchActions::new(
            self.twitch_client_id.clone(),
            creds.access_token,
            creds.user_id,
        ));
        {
            let mut s = self.state.lock().await;
            s.twitch_auth = Some(auth);
            s.twitch_eventsub = Some(eventsub);
            s.twitch_actions = Some(actions);
        }
        self.broadcast(&format!("logged in to Twitch as {}", creds.login))
            .await;
    }

    async fn apply_kick(&self, creds: bks_auth::kick::Credentials) {
        let actions = Arc::new(kick_actions(&creds));
        actions.set_on_refreshed(persist_refreshed_kick()).await;
        {
            let mut s = self.state.lock().await;
            s.kick_actions = Some(actions);
            s.kick_login = Some(creds.username.clone());
        }
        self.broadcast(&format!("logged in to Kick as {}", creds.username))
            .await;
    }

    /// Pushes the new login snapshot to subscribers and logs `notice` (login/logout
    /// is status, not chat — it's logged to stderr, not shown in any tab's feed).
    async fn broadcast(&self, notice: &str) {
        tracing::info!("{notice}");
        let snapshot = self.state.lock().await.snapshot();
        let _ = self.changes.send(snapshot);
    }
}

/// Builds a refresh-aware [`KickActions`] from saved credentials (carrying the
/// refresh token + broker so it can renew an expired access token on a 401).
fn kick_actions(creds: &bks_auth::kick::Credentials) -> KickActions {
    KickActions::new(
        creds.access_token.clone(),
        creds.refresh_token.clone(),
        creds.broker_url.clone(),
    )
}

/// The callback the actions invoke after refreshing their token: merge the rotated
/// `(access, refresh)` tokens into the stored Kick credentials and re-save them, so
/// the renewed login survives a restart. Best-effort — a save failure is logged but
/// doesn't break the in-memory session (which already has the new token).
fn persist_refreshed_kick() -> bks_kick::OnRefreshed {
    Arc::new(
        |access_token: String, refresh_token: String| match bks_auth::kick::load() {
            Ok(Some(mut creds)) => {
                creds.access_token = access_token;
                creds.refresh_token = refresh_token;
                if let Err(err) = bks_auth::kick::save(&creds) {
                    tracing::warn!("could not persist refreshed Kick token: {err:#}");
                }
            }
            _ => tracing::warn!("refreshed Kick token but no stored credentials to update"),
        },
    )
}

/// A Twitch source for a tab: authenticated if logged in, else anonymous.
pub fn twitch_source(auth: Option<TwitchAuth>) -> Arc<TwitchSource> {
    match auth {
        Some(auth) => Arc::new(TwitchSource::authenticated(auth)),
        None => Arc::new(TwitchSource::new()),
    }
}
