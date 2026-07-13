//! The shared rule engine behind [`IgnoreList`](crate::IgnoreList) and
//! [`SuppressList`](crate::SuppressList): both compile the same entry grammar
//! and differ only in what the UI does with a match (hide vs dim).
//!
//! Entry grammar (one rule per raw settings entry):
//! - a plain phrase — case-insensitive substring of the message text
//! - `re:<regex>` — a regex over the message text (compiled case-insensitively;
//!   an invalid one is dropped so a typo can't crash or swallow everything)
//! - `user:<name>` — everything that user sends, on any platform
//! - `user:<platform>/<name>` — that user on one platform only (`twitch/`,
//!   `kick/`, `youtube/`, `tiktok/`; an unknown platform token drops the rule
//!   rather than silently widening it to every platform)
//!
//! User names match exactly (case-insensitive, a leading `@` is tolerated)
//! against the author's login and display name — never as a substring, so
//! `user:bob` doesn't touch `bobby`.

use crate::message::{Author, Message, Platform};
use regex::Regex;

/// Prefix marking a raw entry as a regex rather than a plain phrase.
const REGEX_PREFIX: &str = "re:";
/// Prefix marking a raw entry as a user rule rather than a text rule.
const USER_PREFIX: &str = "user:";

/// One `user:` entry: a name, optionally pinned to a single platform.
#[derive(Clone)]
struct UserRule {
    /// `None` = the rule applies on every platform.
    platform: Option<Platform>,
    /// Lowercased name, compared exactly against login and display name.
    name: String,
}

/// A compiled set of rules. Cheap to clone (regexes are `Arc`-backed
/// internally); rebuild it when the settings list changes.
#[derive(Clone, Default)]
pub(crate) struct TermRules {
    /// Lowercased plain phrases, matched as case-insensitive substrings.
    phrases: Vec<String>,
    /// Compiled regexes (the `re:` entries that parsed).
    regexes: Vec<Regex>,
    /// The `user:` entries that parsed.
    users: Vec<UserRule>,
}

impl TermRules {
    /// Builds the rule set from raw settings entries (see the module docs for
    /// the grammar). Blank and invalid entries are skipped.
    pub(crate) fn new(entries: impl IntoIterator<Item = String>) -> Self {
        let mut rules = Self::default();
        for entry in entries {
            let entry = entry.trim();
            if let Some(pattern) = entry.strip_prefix(REGEX_PREFIX) {
                let pattern = pattern.trim();
                if pattern.is_empty() {
                    continue;
                }
                // Case-insensitive, like the plain-phrase path.
                match Regex::new(&format!("(?i){pattern}")) {
                    Ok(re) => rules.regexes.push(re),
                    Err(err) => tracing::warn!("ignoring invalid regex {pattern:?}: {err}"),
                }
            } else if let Some(user) = entry.strip_prefix(USER_PREFIX) {
                match parse_user_rule(user) {
                    Some(rule) => rules.users.push(rule),
                    None => tracing::warn!("ignoring invalid user rule {entry:?}"),
                }
            } else if !entry.is_empty() {
                rules.phrases.push(entry.to_lowercase());
            }
        }
        rules
    }

    /// Whether the message matches any rule — its text against the phrase/regex
    /// rules, or its author against the user rules.
    pub(crate) fn matches_message(&self, msg: &Message) -> bool {
        self.matches_text(&msg.raw_text) || self.matches_author(msg.platform, &msg.author)
    }

    /// Whether `text` matches any phrase or regex rule.
    pub(crate) fn matches_text(&self, text: &str) -> bool {
        if self.phrases.is_empty() && self.regexes.is_empty() {
            return false;
        }
        let lower = text.to_lowercase();
        self.phrases.iter().any(|p| lower.contains(p))
            || self.regexes.iter().any(|re| re.is_match(text))
    }

    /// Whether the author matches any user rule applicable on `platform`.
    pub(crate) fn matches_author(&self, platform: Platform, author: &Author) -> bool {
        if self.users.is_empty() {
            return false;
        }
        let login = author.login.to_lowercase();
        let display = author.display_name.to_lowercase();
        self.users.iter().any(|rule| {
            rule.platform.is_none_or(|p| p == platform)
                && (rule.name == login || rule.name == display)
        })
    }

    /// Whether the set has no rules (so the UI can skip building/applying it).
    pub(crate) fn is_empty(&self) -> bool {
        self.phrases.is_empty() && self.regexes.is_empty() && self.users.is_empty()
    }
}

/// Parses the part after `user:`: an optional `<platform>/` scope, then the
/// name. `None` when the name is empty or the platform token is unknown.
fn parse_user_rule(raw: &str) -> Option<UserRule> {
    parse_user_parts(raw).map(|(platform, name)| UserRule {
        platform,
        name: name.to_lowercase(),
    })
}

/// Splits a raw `user:` list entry into its parts — the platform scope (`None`
/// = every platform) and the name with its typed case kept (for display).
/// `None` when the entry isn't a valid `user:` rule. The inverse of
/// [`user_entry`].
pub fn parse_user_entry(entry: &str) -> Option<(Option<Platform>, &str)> {
    parse_user_parts(entry.trim().strip_prefix(USER_PREFIX)?)
}

/// Composes the canonical `user:` list entry for a name and platform scope —
/// what the settings editor's User add-mode writes.
pub fn user_entry(platform: Option<Platform>, name: &str) -> String {
    let name = name.trim().trim_start_matches('@');
    match platform {
        Some(p) => format!("{USER_PREFIX}{}/{name}", platform_token(p)),
        None => format!("{USER_PREFIX}{name}"),
    }
}

/// Removes from `entries` every platform-scoped `user:` rule for `name` (any
/// platform, matched case-insensitively) — the narrower rules an unscoped
/// `user:name` makes redundant. Call after adding an all-platforms entry so the
/// list doesn't keep dead `user:twitch/name`/`user:kick/name` siblings. Leaves
/// text/regex entries and other names untouched. Returns whether anything was
/// removed.
pub fn absorb_scoped_user_entries(entries: &mut Vec<String>, name: &str) -> bool {
    let name = name.trim().trim_start_matches('@');
    let before = entries.len();
    entries.retain(|entry| {
        !matches!(
            parse_user_entry(entry),
            Some((Some(_), n)) if n.eq_ignore_ascii_case(name)
        )
    });
    entries.len() != before
}

/// The shared scope/name split behind [`parse_user_rule`] and
/// [`parse_user_entry`] (which lowercase or keep the name respectively).
fn parse_user_parts(raw: &str) -> Option<(Option<Platform>, &str)> {
    let raw = raw.trim();
    let (platform, name) = match raw.split_once('/') {
        Some((scope, rest)) => (Some(parse_platform(scope)?), rest),
        None => (None, raw),
    };
    let name = name.trim().trim_start_matches('@');
    (!name.is_empty()).then_some((platform, name))
}

fn parse_platform(token: &str) -> Option<Platform> {
    match token.trim().to_lowercase().as_str() {
        "twitch" => Some(Platform::Twitch),
        "kick" => Some(Platform::Kick),
        "youtube" => Some(Platform::YouTube),
        "tiktok" => Some(Platform::TikTok),
        _ => None,
    }
}

/// The lowercase platform token used in `user:<platform>/<name>` entries.
fn platform_token(platform: Platform) -> &'static str {
    match platform {
        Platform::Twitch => "twitch",
        Platform::Kick => "kick",
        Platform::YouTube => "youtube",
        Platform::TikTok => "tiktok",
    }
}

#[cfg(test)]
mod tests {
    use super::TermRules;
    use crate::message::{Author, Platform};

    fn rules(entries: &[&str]) -> TermRules {
        TermRules::new(entries.iter().map(|s| s.to_string()))
    }

    fn author(login: &str, display: &str) -> Author {
        Author {
            login: login.into(),
            display_name: display.into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_phrase_is_case_insensitive_substring() {
        let r = rules(&["buy now"]);
        assert!(r.matches_text("hey BUY NOW cheap"));
        assert!(r.matches_text("buy now"));
        assert!(!r.matches_text("buying nowhere"));
    }

    #[test]
    fn regex_entry_matches() {
        let r = rules(&["re:https?://\\S+"]);
        assert!(r.matches_text("check this http://spam.example"));
        assert!(!r.matches_text("no link here"));
    }

    #[test]
    fn regex_is_case_insensitive() {
        let r = rules(&["re:^FREE"]);
        assert!(r.matches_text("free money"));
        assert!(r.matches_text("FREE money"));
    }

    #[test]
    fn invalid_regex_is_dropped_not_matching_everything() {
        // An unclosed group is invalid; it should be skipped, not match all.
        let r = rules(&["re:("]);
        assert!(!r.matches_text("anything"));
        assert!(r.is_empty());
    }

    #[test]
    fn empty_list_never_matches() {
        assert!(!rules(&[]).matches_text("whatever"));
        assert!(!rules(&["", "  ", "re:", "re:   ", "user:", "user:  "]).matches_text("whatever"));
    }

    #[test]
    fn mixed_phrase_and_regex() {
        let r = rules(&["spam", "re:\\d{4,}"]);
        assert!(r.matches_text("this is SPAM"));
        assert!(r.matches_text("number 12345 here"));
        assert!(!r.matches_text("clean message 12"));
    }

    #[test]
    fn user_rule_matches_every_platform() {
        let r = rules(&["user:StreamElements"]);
        let a = author("streamelements", "StreamElements");
        assert!(r.matches_author(Platform::Twitch, &a));
        assert!(r.matches_author(Platform::Kick, &a));
        assert!(!r.matches_author(Platform::Twitch, &author("someone", "Someone")));
    }

    #[test]
    fn user_rule_scoped_to_one_platform() {
        let r = rules(&["user:kick/KickBot"]);
        let a = author("kickbot", "KickBot");
        assert!(r.matches_author(Platform::Kick, &a));
        assert!(!r.matches_author(Platform::Twitch, &a));
    }

    #[test]
    fn user_rule_is_exact_not_substring() {
        let r = rules(&["user:bob"]);
        assert!(r.matches_author(Platform::Twitch, &author("bob", "Bob")));
        assert!(!r.matches_author(Platform::Twitch, &author("bobby", "Bobby")));
    }

    #[test]
    fn user_rule_matches_display_name_and_tolerates_at() {
        let r = rules(&["user:@Nightbot"]);
        assert!(r.matches_author(Platform::Twitch, &author("nightbot_login", "Nightbot")));
        assert!(r.matches_author(Platform::Twitch, &author("nightbot", "夜ボット")));
    }

    #[test]
    fn unknown_platform_scope_is_dropped_not_widened() {
        let r = rules(&["user:twich/typo"]);
        assert!(!r.matches_author(Platform::Twitch, &author("typo", "typo")));
        assert!(r.is_empty());
    }

    #[test]
    fn user_rules_do_not_match_text() {
        let r = rules(&["user:spammer"]);
        assert!(!r.matches_text("spammer said something"));
    }

    #[test]
    fn user_entry_round_trips_through_parse() {
        use super::{parse_user_entry, user_entry};
        let all = user_entry(None, "@StreamElements");
        assert_eq!(all, "user:StreamElements");
        assert_eq!(parse_user_entry(&all), Some((None, "StreamElements")));

        let scoped = user_entry(Some(Platform::Kick), "KickBot");
        assert_eq!(scoped, "user:kick/KickBot");
        assert_eq!(
            parse_user_entry(&scoped),
            Some((Some(Platform::Kick), "KickBot"))
        );

        assert_eq!(parse_user_entry("plain term"), None);
        assert_eq!(parse_user_entry("user:"), None);
        assert_eq!(parse_user_entry("user:twich/typo"), None);
    }

    #[test]
    fn absorb_removes_only_scoped_entries_for_the_name() {
        use super::absorb_scoped_user_entries;
        let mut entries = vec![
            "user:twitch/StreamElements".to_string(),
            "user:kick/streamelements".to_string(),
            "user:StreamElements".to_string(), // the unscoped one stays
            "user:twitch/SomeoneElse".to_string(), // different name stays
            "buy now".to_string(),             // text entry stays
            "re:\\d+".to_string(),             // regex stays
        ];
        let removed = absorb_scoped_user_entries(&mut entries, "@StreamElements");
        assert!(removed);
        assert_eq!(
            entries,
            vec![
                "user:StreamElements".to_string(),
                "user:twitch/SomeoneElse".to_string(),
                "buy now".to_string(),
                "re:\\d+".to_string(),
            ]
        );
        // Nothing to absorb the second time.
        assert!(!absorb_scoped_user_entries(&mut entries, "StreamElements"));
    }
}
