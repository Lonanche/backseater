//! User-defined suppress list: messages whose text matches any entry stay in the
//! feed but are rendered at very low contrast (dimmed but readable), so the eye
//! skips them while they remain available. The middle tier between "show" and
//! [`IgnoreList`](crate::IgnoreList) (which hides entirely).
//!
//! Same "build from settings, test per message" shape as the ignore list and the
//! same entry grammar: a plain phrase (case-insensitive substring) or a `re:`
//! regex. Unlike ignore, suppression is **always** a per-view render concern — a
//! suppressed message must still render, so it is never dropped at ingest.

use regex::Regex;

/// Prefix marking a raw entry as a regex rather than a plain phrase.
pub const REGEX_PREFIX: &str = "re:";

/// A compiled set of suppress rules. Cheap to clone (regexes are `Arc`-backed
/// internally); rebuild it when the settings list changes.
#[derive(Clone, Default)]
pub struct SuppressList {
    /// Lowercased plain phrases, matched as case-insensitive substrings.
    phrases: Vec<String>,
    /// Compiled regexes (the `re:` entries that parsed).
    regexes: Vec<Regex>,
}

impl SuppressList {
    /// Builds the list from raw settings entries. A `re:`-prefixed entry is a
    /// regex (compiled case-insensitively); anything else is a plain phrase.
    /// Blank entries and regexes that fail to compile are skipped.
    pub fn new(entries: impl IntoIterator<Item = String>) -> Self {
        let mut phrases = Vec::new();
        let mut regexes = Vec::new();
        for entry in entries {
            let entry = entry.trim();
            if let Some(pattern) = entry.strip_prefix(REGEX_PREFIX) {
                let pattern = pattern.trim();
                if pattern.is_empty() {
                    continue;
                }
                // Case-insensitive, like the plain-phrase path.
                match Regex::new(&format!("(?i){pattern}")) {
                    Ok(re) => regexes.push(re),
                    Err(err) => tracing::warn!("ignoring invalid regex {pattern:?}: {err}"),
                }
            } else if !entry.is_empty() {
                phrases.push(entry.to_lowercase());
            }
        }
        Self { phrases, regexes }
    }

    /// Whether `text` matches any suppress rule (so the message should be dimmed).
    pub fn matches(&self, text: &str) -> bool {
        if self.phrases.is_empty() && self.regexes.is_empty() {
            return false;
        }
        let lower = text.to_lowercase();
        self.phrases.iter().any(|p| lower.contains(p))
            || self.regexes.iter().any(|re| re.is_match(text))
    }

    /// Whether the list has no rules (so the UI can skip building/applying it).
    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty() && self.regexes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::SuppressList;

    fn list(entries: &[&str]) -> SuppressList {
        SuppressList::new(entries.iter().map(|s| s.to_string()))
    }

    #[test]
    fn plain_phrase_is_case_insensitive_substring() {
        let l = list(&["buy now"]);
        assert!(l.matches("hey BUY NOW cheap"));
        assert!(l.matches("buy now"));
        assert!(!l.matches("buying nowhere"));
    }

    #[test]
    fn regex_entry_matches() {
        let l = list(&["re:https?://\\S+"]);
        assert!(l.matches("check this http://spam.example"));
        assert!(!l.matches("no link here"));
    }

    #[test]
    fn regex_is_case_insensitive() {
        let l = list(&["re:^FREE"]);
        assert!(l.matches("free money"));
        assert!(l.matches("FREE money"));
    }

    #[test]
    fn invalid_regex_is_dropped_not_matching_everything() {
        // An unclosed group is invalid; it should be skipped, not match all.
        let l = list(&["re:("]);
        assert!(!l.matches("anything"));
        assert!(l.is_empty());
    }

    #[test]
    fn empty_list_never_matches() {
        assert!(!list(&[]).matches("whatever"));
        assert!(!list(&["", "  ", "re:", "re:   "]).matches("whatever"));
    }

    #[test]
    fn mixed_phrase_and_regex() {
        let l = list(&["spam", "re:\\d{4,}"]);
        assert!(l.matches("this is SPAM"));
        assert!(l.matches("number 12345 here"));
        assert!(!l.matches("clean message 12"));
    }
}
