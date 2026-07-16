//! Recent chat history, fetched on channel join so the log isn't empty.
//!
//! Twitch IRC sends no backlog, and there's no official Helix endpoint, so we
//! use the public robotty recent-messages API.
//! It returns a list of raw IRC lines (PRIVMSG/USERNOTICE/CLEARCHAT) — exactly
//! the lines `tmi` already parses live. PRIVMSG → [`ChatEvent::Message`],
//! USERNOTICE (subs/raids/announcements) → a `historical`-flagged
//! [`ChatEvent::Event`] (the UI sorts it into the backlog, fades it, and keeps
//! it out of the events panel), and CLEARCHAT → a `historical` ban/timeout fade
//! so a banned user's backlog still renders struck — but silently (no notice
//! row). Lines arrive oldest-first.
//!
//! [`fetch_gap`] is the reconnect variant (the API's `after`/`before` window):
//! it backfills only the messages missed while disconnected and parses them as
//! *live* — they're new to the user, so mentions/events-panel/unread all fire.

use bks_platform::ChatEvent;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tmi::{FromIrc, IrcMessageRef};

use crate::connector::{clearchat_event, privmsg_to_message, usernotice_event};

const RECENT_MESSAGES_URL: &str = "https://recent-messages.robotty.de/api/v2/recent-messages";

#[derive(Deserialize)]
struct RecentMessages {
    #[serde(default)]
    messages: Vec<String>,
}

/// Fetches up to `limit` recent chat events for `channel` (no auth), oldest
/// first, flagged `historical` — the join backlog.
pub async fn fetch_recent(channel: &str, limit: usize) -> anyhow::Result<Vec<ChatEvent>> {
    fetch(channel, limit, None, None, true).await
}

/// Fetches the events between `after` and `before` — the reconnect gap-fill.
/// Unlike the join backlog these parse as **live** (not `historical`): they
/// were missed during the disconnect, so they should feed the events panel /
/// mentions / unread exactly as if they'd arrived on time.
pub async fn fetch_gap(
    channel: &str,
    limit: usize,
    after: DateTime<Utc>,
    before: DateTime<Utc>,
) -> anyhow::Result<Vec<ChatEvent>> {
    fetch(channel, limit, Some(after), Some(before), false).await
}

/// Shared fetch: each raw IRC line is parsed with `tmi` and converted via the
/// same paths as live messages (chat, events, clear-chat); unrecognized lines
/// are dropped. Errors (API down, bad JSON) propagate so the caller can log and
/// continue with no history.
async fn fetch(
    channel: &str,
    limit: usize,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
    historical: bool,
) -> anyhow::Result<Vec<ChatEvent>> {
    let login = bks_core::channel_login(channel);
    let url = history_url(&login, limit, after, before);

    let recent: RecentMessages = crate::http::client()
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // The conversion keys channel off `#name`, matching the live join path.
    let channel = format!("#{login}");
    let events = recent
        .messages
        .iter()
        .filter_map(|line| parse_history_line(&channel, line, historical))
        .collect();
    Ok(events)
}

/// The API URL: `after`/`before` (ms epochs) bound the window for a reconnect
/// gap-fill; omitted on the plain join fetch.
fn history_url(
    login: &str,
    limit: usize,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
) -> String {
    let mut url = format!(
        "{RECENT_MESSAGES_URL}/{}?limit={limit}",
        bks_core::encode_url_component(login)
    );
    if let Some(after) = after {
        url.push_str(&format!("&after={}", after.timestamp_millis()));
    }
    if let Some(before) = before {
        url.push_str(&format!("&before={}", before.timestamp_millis()));
    }
    url
}

/// Parses one raw IRC line into a [`ChatEvent`] via the live conversion paths.
/// With `historical` set (the join backlog) chat and event rows are flagged so
/// the UI sorts + fades them and skips the events panel, and a CLEARCHAT
/// becomes a silent fade; a gap-fill (`historical = false`) parses exactly like
/// live traffic.
fn parse_history_line(channel: &str, line: &str, historical: bool) -> Option<ChatEvent> {
    let irc = IrcMessageRef::parse(line)?;
    // `msg-param-value` (the watch-streak length) is dropped by tmi's parse, so
    // read it off the raw line before `from_irc` consumes it — same value the
    // live loop reads.
    let milestone_value = irc
        .tag(tmi::Tag::from("msg-param-value"))
        .and_then(|v| v.parse().ok());
    match tmi::Message::from_irc(irc).ok()? {
        tmi::Message::Privmsg(pm) => {
            // Historical messages don't carry the live "first message" highlight.
            // The "Highlight My Message" tint (set from the msg-id tag inside
            // privmsg_to_message) is kept, though: unlike an ephemeral event row,
            // a highlighted message is a real chat line that stays highlighted in
            // scrollback (Twitch web keeps it too).
            let mut msg = privmsg_to_message(channel, &pm, false);
            msg.historical = historical;
            Some(ChatEvent::Message(Box::new(msg)))
        }
        tmi::Message::UserNotice(un) => {
            let mut event = usernotice_event(&un, milestone_value)?;
            if historical {
                if let ChatEvent::Event {
                    details, message, ..
                } = &mut event
                {
                    details.historical = true;
                    if let Some(msg) = message {
                        msg.historical = true;
                    }
                }
            }
            Some(event)
        }
        tmi::Message::ClearChat(cc) => Some(clearchat_event(&cc, historical)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_privmsg_history_line() {
        let raw = "@badge-info=;badges=;color=#19E6E6;display-name=qaixx;emotes=;\
                   id=abc;mod=0;room-id=11148817;subscriber=0;\
                   tmi-sent-ts=1594555275886;turbo=0;user-id=40286300;user-type= \
                   :qaixx!qaixx@qaixx.tmi.twitch.tv PRIVMSG #oilrats :hello there";
        match parse_history_line("#oilrats", raw, true) {
            Some(ChatEvent::Message(msg)) => {
                assert_eq!(msg.author.display_name, "qaixx");
                assert_eq!(msg.raw_text, "hello there");
                assert!(msg.historical);
            }
            other => panic!("expected a historical message, got {other:?}"),
        }
    }

    #[test]
    fn gap_fill_privmsg_parses_as_live() {
        let raw = "@badge-info=;badges=;color=#19E6E6;display-name=qaixx;emotes=;\
                   id=abc;mod=0;room-id=11148817;subscriber=0;\
                   tmi-sent-ts=1594555275886;turbo=0;user-id=40286300;user-type= \
                   :qaixx!qaixx@qaixx.tmi.twitch.tv PRIVMSG #oilrats :hello there";
        match parse_history_line("#oilrats", raw, false) {
            Some(ChatEvent::Message(msg)) => assert!(!msg.historical),
            other => panic!("expected a live message, got {other:?}"),
        }
    }

    #[test]
    fn clearchat_history_line_becomes_silent_historical_fade() {
        let raw = "@room-id=11148817;target-user-id=40286300;tmi-sent-ts=1594555275886 \
                   :tmi.twitch.tv CLEARCHAT #oilrats :qaixx";
        match parse_history_line("#oilrats", raw, true) {
            Some(ChatEvent::ClearChat {
                historical: true,
                timestamp: Some(ts),
                ..
            }) => {
                // The clear's real server-side time bounds the fade: a target
                // unbanned since must not have their later messages struck.
                assert_eq!(ts.timestamp_millis(), 1_594_555_275_886);
            }
            other => panic!("expected a historical timestamped clear, got {other:?}"),
        }
    }

    /// A well-formed resub USERNOTICE (tmi's parse needs the full tag set).
    const RESUB_RAW: &str = "@badge-info=subscriber/2;badges=subscriber/0;color=#0000FF;\
                   display-name=Oilrats;emotes=;flags=;id=abc;login=oilrats;mod=0;\
                   msg-id=resub;msg-param-cumulative-months=2;\
                   msg-param-should-share-streak=1;msg-param-streak-months=2;\
                   msg-param-sub-plan-name=Channel;msg-param-sub-plan=1000;\
                   room-id=71092938;subscriber=1;system-msg=resub;\
                   tmi-sent-ts=1581713640019;user-id=21156217;user-type= \
                   :tmi.twitch.tv USERNOTICE #trausi :postySmash";

    #[test]
    fn usernotice_history_line_becomes_historical_event() {
        match parse_history_line("#trausi", RESUB_RAW, true) {
            Some(ChatEvent::Event {
                details,
                message,
                timestamp,
                ..
            }) => {
                assert!(details.historical);
                // The attached resub message is flagged too, so the mentions
                // panel / unread flash skip it like any backlog line.
                assert!(message.expect("resub carries its chat line").historical);
                assert_eq!(timestamp.timestamp_millis(), 1_581_713_640_019);
            }
            other => panic!("expected a historical event, got {other:?}"),
        }
    }

    #[test]
    fn gap_fill_usernotice_parses_as_live() {
        match parse_history_line("#trausi", RESUB_RAW, false) {
            Some(ChatEvent::Event {
                details, message, ..
            }) => {
                assert!(!details.historical);
                assert!(!message.expect("resub carries its chat line").historical);
            }
            other => panic!("expected a live event, got {other:?}"),
        }
    }

    #[test]
    fn history_url_bounds_the_gap_fill_window() {
        let after = DateTime::from_timestamp_millis(1_000).unwrap();
        let before = DateTime::from_timestamp_millis(2_000).unwrap();
        assert_eq!(
            history_url("trausi", 800, Some(after), Some(before)),
            format!("{RECENT_MESSAGES_URL}/trausi?limit=800&after=1000&before=2000")
        );
        assert_eq!(
            history_url("trausi", 800, None, None),
            format!("{RECENT_MESSAGES_URL}/trausi?limit=800")
        );
    }
}
