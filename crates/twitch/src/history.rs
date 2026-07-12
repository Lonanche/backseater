//! Recent chat history, fetched on channel join so the log isn't empty.
//!
//! Twitch IRC sends no backlog, and there's no official Helix endpoint, so we
//! use the public robotty recent-messages API.
//! It returns a list of raw IRC lines (PRIVMSG/USERNOTICE/CLEARCHAT) — exactly
//! the lines `tmi` already parses live. Only *chat* is backfilled: PRIVMSG →
//! [`ChatEvent::Message`], and CLEARCHAT → a `historical` ban/timeout fade so a
//! banned user's backlog still renders struck — but silently (no notice row).
//! USERNOTICE (subs/raids) is dropped entirely: replayed event rows carry no
//! visible timestamp in the log, so a days-old sub or timeout misreads as
//! having just happened on every launch. Lines arrive oldest-first.

use bks_platform::ChatEvent;
use serde::Deserialize;
use tmi::{FromIrc, IrcMessageRef};

use crate::connector::{clearchat_event, privmsg_to_message};

const RECENT_MESSAGES_URL: &str = "https://recent-messages.robotty.de/api/v2/recent-messages";

#[derive(Deserialize)]
struct RecentMessages {
    #[serde(default)]
    messages: Vec<String>,
}

/// Fetches up to `limit` recent chat events for `channel` (no auth), oldest
/// first. Each raw IRC line is parsed with `tmi` and converted via the same paths
/// as live messages (chat, events, clear-chat); unrecognized lines are dropped.
/// Errors (API down, bad JSON) propagate so the caller can log and continue with
/// no history.
pub async fn fetch_recent(channel: &str, limit: usize) -> anyhow::Result<Vec<ChatEvent>> {
    let login = bks_core::channel_login(channel);
    let url = format!(
        "{RECENT_MESSAGES_URL}/{}?limit={limit}",
        bks_core::encode_url_component(&login)
    );

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
        .filter_map(|line| parse_history_line(&channel, line))
        .collect();
    Ok(events)
}

/// Parses one raw IRC line into a [`ChatEvent`]. Chat messages are flagged
/// `historical` so the UI sorts + fades them; a CLEARCHAT becomes a silent
/// (`historical`) fade; USERNOTICE is dropped (see the module doc — replayed
/// event rows misread as fresh).
fn parse_history_line(channel: &str, line: &str) -> Option<ChatEvent> {
    let irc = IrcMessageRef::parse(line)?;
    match tmi::Message::from_irc(irc).ok()? {
        tmi::Message::Privmsg(pm) => {
            // Historical messages don't carry the live "first message" highlight.
            let mut msg = privmsg_to_message(channel, &pm, false);
            msg.historical = true;
            Some(ChatEvent::Message(Box::new(msg)))
        }
        tmi::Message::ClearChat(cc) => Some(clearchat_event(&cc, true)),
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
        match parse_history_line("#oilrats", raw) {
            Some(ChatEvent::Message(msg)) => {
                assert_eq!(msg.author.display_name, "qaixx");
                assert_eq!(msg.raw_text, "hello there");
                assert!(msg.historical);
            }
            other => panic!("expected a historical message, got {other:?}"),
        }
    }

    #[test]
    fn clearchat_history_line_becomes_silent_historical_fade() {
        let raw = "@room-id=11148817;target-user-id=40286300;tmi-sent-ts=1594555275886 \
                   :tmi.twitch.tv CLEARCHAT #oilrats :qaixx";
        match parse_history_line("#oilrats", raw) {
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

    #[test]
    fn usernotice_history_line_is_dropped() {
        // A replayed sub/raid row carries no visible timestamp in the log, so it
        // would misread as fresh — history backfills chat only.
        let raw = "@badge-info=subscriber/2;badges=subscriber/0;color=#0000FF;\
                   display-name=Oilrats;emotes=;flags=;id=abc;login=oilrats;mod=0;\
                   msg-id=resub;msg-param-cumulative-months=2;\
                   msg-param-sub-plan=1000;room-id=71092938;subscriber=1;\
                   system-msg=resub;tmi-sent-ts=1581713640019;user-id=21156217;\
                   user-type= :tmi.twitch.tv USERNOTICE #trausi :postySmash";
        assert!(parse_history_line("#trausi", raw).is_none());
    }
}
