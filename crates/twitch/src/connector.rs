use async_trait::async_trait;
use bks_core::{plural, Author, Badge, Color, Message, MessageElement, Platform, ReplyParent};
use bks_platform::{ChannelMeta, ChatEvent, ChatSource, ChatStream, EventDetails, EventKind};
use std::sync::Mutex;
use tokio::sync::mpsc;

use crate::builder::build_privmsg_elements;
use crate::irc_manager::{self, Registration};

/// Login info for authenticated chat: the IRC nick + `oauth:<token>` pass.
#[derive(Clone)]
pub struct TwitchAuth {
    pub login: String,
    pub oauth_pass: String,
}

/// Twitch IRC connector. Anonymous by default (read-only); with [`TwitchAuth`]
/// the joined connection is logged in and can send.
///
/// This is now a thin handle over the app-wide shared IRC connection
/// ([`crate::irc_manager`]): every tab's `TwitchSource`
/// registers its channel onto **one shared read + one shared write socket** per
/// logged-in user, instead of opening its own socket. `join` registers the
/// channel and returns its event stream; `send`/`send_reply` enqueue onto the
/// shared write client; [`cancel`](Self::cancel) drops the registration (PARTs
/// the channel) — used when swapping connections on login/logout.
pub struct TwitchSource {
    auth: Option<TwitchAuth>,
    /// The live registration on the shared connection, set on `join`. Dropping it
    /// PARTs the channel; `cancel` clears it explicitly.
    registration: Mutex<Option<Registration>>,
}

impl Default for TwitchSource {
    fn default() -> Self {
        Self {
            auth: None,
            registration: Mutex::new(None),
        }
    }
}

impl TwitchSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// An authenticated connector that can send messages as `auth.login`.
    pub fn authenticated(auth: TwitchAuth) -> Self {
        Self {
            auth: Some(auth),
            ..Default::default()
        }
    }

    /// Tears the connection down: drops this tab's registration on the shared
    /// connection, which PARTs the channel. Used when swapping connections on
    /// login/logout so no channel stays joined for a gone tab.
    pub fn cancel(&self) {
        *self.registration.lock().unwrap() = None;
    }
}

#[async_trait]
impl ChatSource for TwitchSource {
    async fn join(&self, channel: &str) -> anyhow::Result<ChatStream> {
        let channel = normalize_channel(channel);
        let (tx, rx) = mpsc::unbounded_channel();
        let reg = irc_manager::register(self.auth.clone(), channel, tx);
        *self.registration.lock().unwrap() = Some(reg);
        Ok(rx)
    }

    async fn send(
        &self,
        _channel: &str,
        text: &str,
        reply_parent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.enqueue(text, reply_parent_id.map(str::to_string))
    }
}

impl TwitchSource {
    /// Sends a reply that threads under `reply_parent_id`. The threaded PRIVMSG
    /// comes back on the read connection with its reply tags (Twitch delivers our
    /// own sent messages to the separate read session), so the "replying to" line
    /// renders from the real message — no local echo. `_reply_parent` is accepted
    /// for API symmetry with the Kick path but no longer needed here.
    pub async fn send_reply(
        &self,
        _channel: &str,
        text: &str,
        reply_parent_id: &str,
        _reply_parent: bks_core::ReplyParent,
    ) -> anyhow::Result<()> {
        self.enqueue(text, Some(reply_parent_id.to_string()))
    }

    /// Hands a message to the shared write client via this tab's registration.
    fn enqueue(&self, text: &str, reply_parent_id: Option<String>) -> anyhow::Result<()> {
        let guard = self.registration.lock().unwrap();
        let reg = guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("not connected"))?;
        if self.auth.is_none() {
            anyhow::bail!("not logged in (log in via Settings → Account)");
        }
        reg.send(text.to_string(), reply_parent_id);
        Ok(())
    }
}

/// Twitch channels are joined as `#name`, lowercased.
fn normalize_channel(channel: &str) -> String {
    format!("#{}", bks_core::channel_login(channel))
}

/// Builds a [`ChannelMeta`] from a channel name (`#name` or `name`) and its
/// numeric room id, shared by the manager's ROOMSTATE/PRIVMSG handlers.
pub(crate) fn build_channel_meta(channel: &str, id: &str) -> ChannelMeta {
    ChannelMeta {
        platform: Platform::Twitch,
        id: id.to_string(),
        name: bks_core::strip_channel(channel).to_string(),
    }
}

/// Converts a CLEARCHAT (a ban/timeout that clears a user's messages, or a whole
/// chat clear) into a [`ChatEvent::ClearChat`]. Shared by the live loop
/// (`historical = false`) and the history backfill (`true` — fades the backlog
/// silently, no notice for a timeout that predates this session).
pub(crate) fn clearchat_event(cc: &tmi::msg::ClearChat<'_>, historical: bool) -> ChatEvent {
    ChatEvent::ClearChat {
        platform: Platform::Twitch,
        user: cc.target().map(|u| u.to_string()),
        historical,
    }
}

/// Converts a USERNOTICE (sub/resub/gift/raid/announcement) into a highlighted
/// [`ChatEvent::Event`]. Twitch ships a ready-made `system-msg` (the exact text
/// twitch.tv shows); it's public, so we surface it as the event's `text`. On a
/// sub/resub the chatter can attach a chat message — that becomes a full
/// [`Message`] (author, badges, timestamp, elements built through the same path
/// as a PRIVMSG, so its native emotes render inline) shown under the system text
/// like a normal chat line. `None` when there's no system-msg (nothing to show).
/// Shared by the live loop and history backfill.
pub(crate) fn usernotice_event(un: &tmi::msg::UserNotice<'_>) -> Option<ChatEvent> {
    let text = un.system_message()?.to_string();
    // The attached sub message (if any) carries native emote positions in the
    // USERNOTICE `emotes` tag, indexed against this text — same as a PRIVMSG, so
    // pass it un-trimmed (trimming would shift those positions).
    let message = un
        .text()
        .filter(|s| !s.trim().is_empty())
        .and_then(|user_msg| {
            let sender = un.sender()?;
            Some(Box::new(Message {
                id: un.message_id().to_string(),
                platform: Platform::Twitch,
                channel: bks_core::strip_channel(un.channel()).to_string(),
                timestamp: un.timestamp(),
                author: Author {
                    login: sender.login().to_string(),
                    display_name: sender.name().to_string(),
                    color: un.color().and_then(Color::from_hex),
                    badges: un.badges().map(badge_from_irc).collect(),
                    user_id: sender.id().to_string(),
                    paint: None,
                },
                elements: build_privmsg_elements(user_msg, un.raw_emotes(), None),
                raw_text: user_msg.to_string(),
                reply: None,
                first_message: false,
                historical: false,
            }))
        });
    Some(ChatEvent::Event {
        platform: Platform::Twitch,
        kind: usernotice_kind(un),
        text,
        timestamp: un.timestamp(),
        message,
        details: usernotice_details(un),
    })
}

/// The login key anonymous gifters share, so an anonymous mass gift's
/// announcement and its per-recipient events still group together.
const ANON_GIFTER: &str = "ananonymousgifter";

/// Builds the structured [`EventDetails`] for a USERNOTICE from tmi's parsed
/// event data: a condensed line for the events panel ("resubbed · 12 mo ·
/// Tier 1") plus the gifter/recipient/count fields that let the store tie a
/// mass gift's individual events to their announcement. Events without
/// structured data (announcements, rituals, …) get only the actor — the panel
/// falls back to the full `system-msg` text.
fn usernotice_details(un: &tmi::msg::UserNotice<'_>) -> EventDetails {
    use tmi::msg::user_notice::Event;
    let actor = un.sender().map(|s| s.name().to_string());
    if un.event_id() == "viewermilestone" {
        // tmi has no structured watch-streak variant (it parses as
        // `Event::Unknown`, which tmi treats as anonymous — `sender()` is
        // None), so both the chatter and the streak length come out of the
        // system text ("viewer watched 7 consecutive streams this month and
        // sparked a watch streak!"): the leading token is the name, the first
        // number the length. No number → the panel falls back to that full
        // text.
        let sys = un.system_message().unwrap_or_default();
        let compact = first_number(&sys)
            .map(|n| format!("watch streak · {n} {}", plural(n, "stream", "streams")));
        let actor = compact
            .is_some()
            .then(|| sys.split_whitespace().next().map(str::to_string))
            .flatten();
        return EventDetails {
            actor,
            compact,
            ..Default::default()
        };
    }
    let gifter_key = || {
        Some(
            un.sender()
                .map(|s| s.login().to_lowercase())
                .unwrap_or_else(|| ANON_GIFTER.to_string()),
        )
    };
    match un.event() {
        Event::SubOrResub(sub) => {
            let compact = if sub.is_resub() {
                format!(
                    "resubbed · {} mo{}",
                    sub.cumulative_months().max(1),
                    tier_suffix(sub.sub_plan())
                )
            } else {
                format!("subscribed{}", tier_suffix(sub.sub_plan()))
            };
            EventDetails {
                actor,
                compact: Some(compact),
                ..Default::default()
            }
        }
        Event::SubGift(gift) => EventDetails {
            actor: actor.or_else(|| Some("An anonymous user".to_string())),
            compact: Some(format!(
                "gifted a sub to {}{}",
                gift.recipient().name(),
                tier_suffix(gift.sub_plan())
            )),
            gifter: gifter_key(),
            recipient: Some(gift.recipient().name().to_string()),
            ..Default::default()
        },
        Event::SubMysteryGift(gift) => {
            let mut compact = format!(
                "gifted {} {}{}",
                gift.count(),
                if gift.count() == 1 { "sub" } else { "subs" },
                tier_suffix(gift.sub_plan())
            );
            // The gifter's channel-lifetime gift count rides the same notice.
            if let Some(total) = gift.sender_total_gifts() {
                compact.push_str(&format!(
                    " · {} total",
                    bks_core::format_count(u64::from(total))
                ));
            }
            EventDetails {
                actor,
                compact: Some(compact),
                gift_count: Some(gift.count() as u32),
                gifter: gifter_key(),
                ..Default::default()
            }
        }
        Event::AnonSubMysteryGift(gift) => EventDetails {
            actor: Some("An anonymous user".to_string()),
            compact: Some(format!(
                "gifted {} {}{}",
                gift.count(),
                if gift.count() == 1 { "sub" } else { "subs" },
                tier_suffix(gift.sub_plan())
            )),
            gift_count: Some(gift.count() as u32),
            gifter: Some(ANON_GIFTER.to_string()),
            ..Default::default()
        },
        Event::Raid(raid) => EventDetails {
            actor,
            compact: Some(format!(
                "raided · {} viewers",
                bks_core::format_count(raid.viewer_count())
            )),
            ..Default::default()
        },
        Event::GiftPaidUpgrade(_) | Event::AnonGiftPaidUpgrade(_) => EventDetails {
            actor,
            compact: Some("continued their gifted sub".to_string()),
            ..Default::default()
        },
        Event::BitsBadgeTier(tier) => EventDetails {
            actor,
            compact: Some(format!(
                "unlocked the {} bits badge",
                bks_core::format_count(tier.tier())
            )),
            ..Default::default()
        },
        _ => EventDetails {
            actor,
            ..Default::default()
        },
    }
}

/// The first run of ASCII digits in `text`, as a number.
fn first_number(text: &str) -> Option<u64> {
    text.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse()
        .ok()
}

/// " · Tier N" / " · Prime" for a `msg-param-sub-plan` value, empty when
/// unrecognized.
fn tier_suffix(plan: &str) -> &'static str {
    match plan {
        "1000" => " · Tier 1",
        "2000" => " · Tier 2",
        "3000" => " · Tier 3",
        "Prime" => " · Prime",
        _ => "",
    }
}


/// Classifies a USERNOTICE into an [`EventKind`] so the UI can filter events.
/// tmi's `Event` enum splits the common cases; the watch-streak milestone has no
/// tmi variant (it parses as `Unknown`), so we key it off the raw `event_id`
/// (`viewermilestone`). Gift variants fold into `Gift`, raid into `Raid`,
/// bits-badge tiers into `Bits`, and everything else into `Other`.
fn usernotice_kind(un: &tmi::msg::UserNotice<'_>) -> EventKind {
    use tmi::msg::user_notice::Event;
    if un.event_id() == "viewermilestone" {
        return EventKind::WatchStreak;
    }
    match un.event() {
        Event::SubOrResub(_) => EventKind::Sub,
        Event::SubGift(_) | Event::SubMysteryGift(_) | Event::AnonSubMysteryGift(_) => {
            EventKind::Gift
        }
        Event::Raid(_) => EventKind::Raid,
        Event::BitsBadgeTier(_) => EventKind::Bits,
        _ => EventKind::Other,
    }
}

pub(crate) fn privmsg_to_message(
    channel: &str,
    pm: &tmi::msg::Privmsg<'_>,
    first_message: bool,
) -> Message {
    let color = pm.color().and_then(Color::from_hex);

    // The IRC tag gives each badge as `set-id/version` (e.g. `subscriber/6`,
    // `vip/1`). We store that as the badge `id`; the image `url` is filled in
    // later by the bridge from the channel's badge map (fetched without auth).
    let badges = pm.badges().map(badge_from_irc).collect();

    let author = Author {
        login: pm.sender().login().to_string(),
        display_name: pm.sender().name().to_string(),
        color,
        badges,
        user_id: pm.sender().id().to_string(),
        paint: None,
    };

    // On a reply, Twitch prepends `@ParentName ` to the body; carry the parent's
    // author + text as a `ReplyParent` so the UI can show a "replying to" line.
    let reply = pm.reply_to().map(|r| ReplyParent {
        author: r.sender().name().to_string(),
        text: r.text().to_string(),
    });

    // Twitch message bodies carry no per-run color; the whole run is uncolored.
    // The `@mention ` reply prefix is left in the body: Twitch's native `emotes`
    // tag positions are indexed against the full text *including* that prefix, so
    // stripping it here would misalign every emote range. The reply context above
    // it is the cue; the leading mention is harmless.
    let elements: Vec<MessageElement> = build_privmsg_elements(pm.text(), pm.raw_emotes(), None);

    Message {
        id: pm.id().to_string(),
        platform: Platform::Twitch,
        channel: bks_core::strip_channel(channel).to_string(),
        // The IRC `tmi-sent-ts` tag carries the real send time (so history shows
        // when each message was sent, not "now"); fall back to now if absent.
        timestamp: pm.timestamp(),
        author,
        elements,
        raw_text: pm.text().to_string(),
        reply,
        first_message,
        historical: false,
    }
}

/// Our own channel-specific identity, captured from USERSTATE. Now only used to
/// derive our moderator status (the read connection delivers our sent messages
/// back with full identity, so no local echo needs our badges/name/color).
pub(crate) struct SelfState {
    badges: Vec<Badge>,
}

impl SelfState {
    pub(crate) fn from_userstate(us: &tmi::msg::UserState<'_>) -> Self {
        Self {
            badges: us.badges().map(badge_from_irc).collect(),
        }
    }

    /// Whether we can moderate this channel: our badges include broadcaster or
    /// moderator. The badge id is `set/version` (e.g. `moderator/1`).
    pub(crate) fn is_moderator(&self) -> bool {
        self.badges.iter().any(|b| {
            let set = b.id.split('/').next().unwrap_or("");
            set == "broadcaster" || set == "moderator"
        })
    }

    /// Whether we own this channel (broadcaster badge). Only the broadcaster can
    /// grant/revoke mod + VIP, so the UI gates those buttons on this.
    pub(crate) fn is_broadcaster(&self) -> bool {
        self.badges
            .iter()
            .any(|b| b.id.split('/').next().unwrap_or("") == "broadcaster")
    }
}


/// Converts one tmi badge into our `set-id/version` keyed [`Badge`] (url filled
/// in later). Kept separate so the id format can be unit-tested against the
/// badge map's keys.
fn badge_from_irc(b: &tmi::Badge<'_>) -> Badge {
    let data = b.as_badge_data();
    Badge {
        id: format!("{}/{}", data.name(), data.version()),
        url: String::new(),
        title: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmi::{FromIrc, IrcMessageRef};

    fn badge_ids(raw: &str) -> Vec<String> {
        let irc = IrcMessageRef::parse(raw).unwrap();
        let pm = tmi::msg::Privmsg::from_irc(irc).unwrap();
        pm.badges().map(badge_from_irc).map(|b| b.id).collect()
    }

    fn parse_privmsg(raw: &str) -> tmi::msg::Privmsg<'_> {
        let irc = IrcMessageRef::parse(raw).unwrap();
        tmi::msg::Privmsg::from_irc(irc).unwrap()
    }

    #[test]
    fn reply_parent_is_captured() {
        // Real-shape reply line (from tmi's own snapshot tests).
        let raw = "@badge-info=;badges=;client-nonce=cd56193132f934ac71b4d5ac488d4bd6;\
                   color=;display-name=Qaixx;emotes=;first-msg=0;flags=;\
                   id=5b4f63a9-776f-4fce-bf3c-d9707f52e32d;mod=0;\
                   reply-parent-display-name=Posty;reply-parent-msg-body=hello;\
                   reply-parent-msg-id=6b13e51b-7ecb-43b5-ba5b-2bb5288df696;\
                   reply-parent-user-id=37940952;reply-parent-user-login=posty;\
                   reply-thread-parent-msg-id=6b13e51b-7ecb-43b5-ba5b-2bb5288df696;\
                   reply-thread-parent-user-login=posty;returning-chatter=0;\
                   room-id=37940952;subscriber=0;tmi-sent-ts=1673925983585;turbo=0;\
                   user-id=133651738;user-type= \
                   :qaixx!qaixx@qaixx.tmi.twitch.tv PRIVMSG #posty :@Posty yes";
        let msg = privmsg_to_message("#posty", &parse_privmsg(raw), false);
        let reply = msg.reply.expect("reply parent");
        assert_eq!(reply.author, "Posty");
        assert_eq!(reply.text, "hello");
    }

    #[test]
    fn non_reply_has_no_parent() {
        let raw = "@badge-info=;badges=;color=#19E6E6;display-name=qaixx;emotes=;\
                   id=abc;mod=0;room-id=11148817;subscriber=0;\
                   tmi-sent-ts=1594555275886;turbo=0;user-id=40286300;user-type= \
                   :qaixx!qaixx@qaixx.tmi.twitch.tv PRIVMSG #lonanche :just chatting";
        let msg = privmsg_to_message("#lonanche", &parse_privmsg(raw), false);
        assert!(msg.reply.is_none());
    }

    /// `first-msg=1` flags a chatter's first-ever message in the channel; reading
    /// it (in `run_client`) is by raw tag since tmi doesn't surface it.
    fn first_msg_flag(raw: &str) -> bool {
        IrcMessageRef::parse(raw).unwrap().tag(tmi::Tag::FirstMsg) == Some("1")
    }

    #[test]
    fn first_message_tag_is_read() {
        let with = "@badge-info=;badges=;color=;display-name=newbie;emotes=;first-msg=1;\
                    flags=;id=abc;mod=0;room-id=11148817;subscriber=0;\
                    tmi-sent-ts=1594555275886;turbo=0;user-id=40286300;user-type= \
                    :newbie!newbie@newbie.tmi.twitch.tv PRIVMSG #oilrats :hello";
        let without = with.replace("first-msg=1", "first-msg=0");
        assert!(first_msg_flag(with));
        assert!(!first_msg_flag(&without));
    }

    #[test]
    fn subscriber_and_other_badge_ids_match_irc_versions() {
        // Real-shape line: a moderator + 12-month subscriber.
        let raw = "@badge-info=subscriber/14;badges=moderator/1,subscriber/12;\
                   color=#19E6E6;display-name=qaixx;emotes=;id=abc;mod=1;\
                   room-id=11148817;subscriber=1;tmi-sent-ts=1594555275886;\
                   turbo=0;user-id=40286300;user-type=mod \
                   :qaixx!qaixx@qaixx.tmi.twitch.tv PRIVMSG #trausi :hi";
        assert_eq!(badge_ids(raw), vec!["moderator/1", "subscriber/12"]);
    }

    fn self_state(badge_ids: &[&str]) -> SelfState {
        SelfState {
            badges: badge_ids
                .iter()
                .map(|id| Badge {
                    id: id.to_string(),
                    url: String::new(),
                    title: None,
                })
                .collect(),
        }
    }

    #[test]
    fn is_moderator_detects_mod_and_broadcaster() {
        assert!(self_state(&["moderator/1"]).is_moderator());
        assert!(self_state(&["broadcaster/1"]).is_moderator());
        assert!(self_state(&["subscriber/12", "moderator/1"]).is_moderator());
        assert!(!self_state(&["subscriber/12", "vip/1"]).is_moderator());
        assert!(!self_state(&[]).is_moderator());
    }

    #[test]
    fn is_broadcaster_only_for_channel_owner() {
        assert!(self_state(&["broadcaster/1"]).is_broadcaster());
        assert!(!self_state(&["moderator/1"]).is_broadcaster());
        assert!(!self_state(&["subscriber/12", "vip/1"]).is_broadcaster());
        assert!(!self_state(&[]).is_broadcaster());
    }

    fn parse_usernotice(raw: &str) -> tmi::msg::UserNotice<'_> {
        let irc = IrcMessageRef::parse(raw).unwrap();
        tmi::msg::UserNotice::from_irc(irc).unwrap()
    }

    #[test]
    fn watch_streak_milestone_classifies_as_watch_streak() {
        // A `viewermilestone` USERNOTICE has no dedicated tmi event (it parses as
        // `Unknown`), so the kind must come from the raw `event_id`.
        let raw = "@badge-info=subscriber/3;badges=subscriber/3;color=;\
                   display-name=viewer;emotes=;flags=;id=abc;login=viewer;mod=0;\
                   msg-id=viewermilestone;msg-param-category=watch-streak;\
                   msg-param-value=7;msg-param-id=xyz;room-id=11148817;subscriber=1;\
                   system-msg=viewer\\swatched\\s7\\sconsecutive\\sstreams\\sthis\\smonth\\sand\\ssparked\\sa\\swatch\\sstreak!;\
                   tmi-sent-ts=1594555275886;user-id=40286300;user-type= \
                   :tmi.twitch.tv USERNOTICE #posty :hi";
        let un = parse_usernotice(raw);
        assert_eq!(usernotice_kind(&un), EventKind::WatchStreak);
    }

    #[test]
    fn resub_still_classifies_as_sub() {
        let raw = "@badge-info=subscriber/2;badges=subscriber/0;color=#0000FF;\
                   display-name=Oilrats;emotes=;flags=;id=abc;login=oilrats;mod=0;\
                   msg-id=resub;msg-param-cumulative-months=2;\
                   msg-param-should-share-streak=1;msg-param-streak-months=2;\
                   msg-param-sub-plan-name=Channel;msg-param-sub-plan=1000;\
                   room-id=71092938;subscriber=1;system-msg=resub;\
                   tmi-sent-ts=1581713640019;user-id=21156217;user-type= \
                   :tmi.twitch.tv USERNOTICE #qaixx :peepoEvil";
        let un = parse_usernotice(raw);
        assert_eq!(usernotice_kind(&un), EventKind::Sub);
    }

    #[test]
    fn resub_attached_message_becomes_full_message() {
        let raw = "@badge-info=subscriber/2;badges=subscriber/0;color=#0000FF;\
                   display-name=Oilrats;emotes=;flags=;id=abc;login=oilrats;mod=0;\
                   msg-id=resub;msg-param-cumulative-months=2;\
                   msg-param-should-share-streak=1;msg-param-streak-months=2;\
                   msg-param-sub-plan-name=Channel;msg-param-sub-plan=1000;\
                   room-id=71092938;subscriber=1;system-msg=resub;\
                   tmi-sent-ts=1581713640019;user-id=21156217;user-type= \
                   :tmi.twitch.tv USERNOTICE #qaixx :peepoEvil";
        let un = parse_usernotice(raw);
        match usernotice_event(&un) {
            Some(ChatEvent::Event {
                timestamp,
                message: Some(msg),
                ..
            }) => {
                assert_eq!(msg.author.display_name, "Oilrats");
                assert_eq!(msg.author.login, "oilrats");
                assert_eq!(msg.raw_text, "peepoEvil");
                assert_eq!(msg.channel, "qaixx");
                assert_eq!(msg.timestamp, timestamp);
                assert_eq!(msg.author.badges.len(), 1);
            }
            other => panic!("expected an event with an attached message, got {other:?}"),
        }
    }

    #[test]
    fn sub_without_attached_text_has_no_message() {
        let raw = "@badge-info=subscriber/0;badges=subscriber/0,premium/1;color=;\
                   display-name=posty;emotes=;flags=;id=abc;\
                   login=posty;mod=0;msg-id=sub;\
                   msg-param-cumulative-months=1;msg-param-months=0;\
                   msg-param-should-share-streak=0;msg-param-sub-plan-name=Channel;\
                   msg-param-sub-plan=Prime;room-id=71092938;subscriber=1;\
                   system-msg=posty\\ssubscribed\\swith\\sTwitch\\sPrime.;\
                   tmi-sent-ts=1582685713242;user-id=224005980;user-type= \
                   :tmi.twitch.tv USERNOTICE #oilrats";
        let un = parse_usernotice(raw);
        match usernotice_event(&un) {
            Some(ChatEvent::Event {
                text,
                message: None,
                ..
            }) => {
                assert_eq!(text, "posty subscribed with Twitch Prime.");
            }
            other => panic!("expected a message-less event, got {other:?}"),
        }
    }

    #[test]
    fn watch_streak_details_are_condensed() {
        let raw = "@badge-info=subscriber/3;badges=subscriber/3;color=;\
                   display-name=viewer;emotes=;flags=;id=abc;login=viewer;mod=0;\
                   msg-id=viewermilestone;msg-param-category=watch-streak;\
                   msg-param-value=7;msg-param-id=xyz;room-id=11148817;subscriber=1;\
                   system-msg=viewer\\swatched\\s7\\sconsecutive\\sstreams\\sthis\\smonth\\sand\\ssparked\\sa\\swatch\\sstreak!;\
                   tmi-sent-ts=1594555275886;user-id=40286300;user-type= \
                   :tmi.twitch.tv USERNOTICE #posty :hi";
        let details = usernotice_details(&parse_usernotice(raw));
        assert_eq!(details.actor.as_deref(), Some("viewer"));
        assert_eq!(details.compact.as_deref(), Some("watch streak · 7 streams"));
    }

    #[test]
    fn resub_details_are_condensed() {
        let raw = "@badge-info=subscriber/2;badges=subscriber/0;color=#0000FF;\
                   display-name=Oilrats;emotes=;flags=;id=abc;login=oilrats;mod=0;\
                   msg-id=resub;msg-param-cumulative-months=12;\
                   msg-param-should-share-streak=1;msg-param-streak-months=2;\
                   msg-param-sub-plan-name=Channel;msg-param-sub-plan=1000;\
                   room-id=71092938;subscriber=1;system-msg=resub;\
                   tmi-sent-ts=1581713640019;user-id=21156217;user-type= \
                   :tmi.twitch.tv USERNOTICE #qaixx :peepoEvil";
        let details = usernotice_details(&parse_usernotice(raw));
        assert_eq!(details.actor.as_deref(), Some("Oilrats"));
        assert_eq!(details.compact.as_deref(), Some("resubbed · 12 mo · Tier 1"));
        assert_eq!(details.gift_count, None);
        assert_eq!(details.gifter, None);
    }

    #[test]
    fn mystery_gift_details_mark_a_batch() {
        let raw = "@badges=;color=;display-name=Rich;emotes=;flags=;id=abc;\
                   login=rich;mod=0;msg-id=submysterygift;\
                   msg-param-mass-gift-count=50;msg-param-origin-id=xyz;\
                   msg-param-sender-count=100;msg-param-sub-plan=1000;\
                   room-id=1;subscriber=0;system-msg=gift;\
                   tmi-sent-ts=1581713640019;user-id=2;user-type= \
                   :tmi.twitch.tv USERNOTICE #chan";
        let details = usernotice_details(&parse_usernotice(raw));
        assert_eq!(details.actor.as_deref(), Some("Rich"));
        assert_eq!(
            details.compact.as_deref(),
            Some("gifted 50 subs · Tier 1 · 100 total")
        );
        assert_eq!(details.gift_count, Some(50));
        assert_eq!(details.gifter.as_deref(), Some("rich"));
    }

    #[test]
    fn sub_gift_details_carry_the_recipient() {
        let raw = "@badges=;color=;display-name=Rich;emotes=;flags=;id=abc;\
                   login=rich;mod=0;msg-id=subgift;msg-param-months=1;\
                   msg-param-recipient-display-name=Lucky;\
                   msg-param-recipient-id=3;msg-param-recipient-user-name=lucky;\
                   msg-param-sub-plan-name=Channel;msg-param-sub-plan=1000;\
                   msg-param-gift-months=1;room-id=1;subscriber=0;\
                   system-msg=gift;tmi-sent-ts=1581713640019;user-id=2;user-type= \
                   :tmi.twitch.tv USERNOTICE #chan";
        let details = usernotice_details(&parse_usernotice(raw));
        assert_eq!(details.actor.as_deref(), Some("Rich"));
        assert_eq!(
            details.compact.as_deref(),
            Some("gifted a sub to Lucky · Tier 1")
        );
        assert_eq!(details.gifter.as_deref(), Some("rich"));
        assert_eq!(details.recipient.as_deref(), Some("Lucky"));
        assert_eq!(details.gift_count, None);
    }

}
