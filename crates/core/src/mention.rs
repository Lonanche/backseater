//! Whole-word, case-insensitive matching of a message against a set of mention
//! terms (your account name, plus user-defined terms like "mods").
//!
//! A term matches when it appears as a standalone word in the message text — so
//! "alice" matches `alice`, `@alice`, and `Alice`, but not `aliceX`. A
//! leading `@` on either side is ignored. This is pure (no GUI/state) so the UI
//! can build a matcher from the current login names + settings and test it.

/// A compiled set of lowercase mention terms. Cheap to build; clone freely.
#[derive(Clone, Default)]
pub struct MentionMatcher {
    terms: Vec<String>,
}

impl MentionMatcher {
    /// Builds a matcher from raw terms. Each term is trimmed, lowercased, and has
    /// a leading `@` removed; blank terms are dropped. Duplicates are harmless.
    pub fn new(terms: impl IntoIterator<Item = String>) -> Self {
        let terms = terms
            .into_iter()
            .map(|t| t.trim().trim_start_matches('@').to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        Self { terms }
    }

    /// Whether any term appears as a standalone word in `text`.
    pub fn matches(&self, text: &str) -> bool {
        if self.terms.is_empty() {
            return false;
        }
        text.split(|c: char| !is_word_char(c))
            .map(|word| word.trim_start_matches('@'))
            .filter(|word| !word.is_empty())
            .any(|word| {
                self.terms
                    .iter()
                    .any(|term| word.eq_ignore_ascii_case(term))
            })
    }
}

/// Characters that form a word for mention matching. `@` is treated as a
/// separator so `@name` splits to `name`; underscores are kept (valid in names).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::MentionMatcher;

    fn m(terms: &[&str]) -> MentionMatcher {
        MentionMatcher::new(terms.iter().map(|s| s.to_string()))
    }

    #[test]
    fn matches_bare_name() {
        assert!(m(&["alice"]).matches("hey alice how are you"));
    }

    #[test]
    fn matches_at_prefixed() {
        assert!(m(&["alice"]).matches("yo @alice check this"));
    }

    #[test]
    fn case_insensitive() {
        assert!(m(&["alice"]).matches("hello ALICE"));
        assert!(m(&["ALICE"]).matches("hello alice"));
    }

    #[test]
    fn term_with_at_is_normalized() {
        assert!(m(&["@alice"]).matches("hi alice"));
    }

    #[test]
    fn no_substring_match() {
        assert!(!m(&["alice"]).matches("myalicefan says hi"));
        assert!(!m(&["mod"]).matches("modern problems"));
    }

    #[test]
    fn custom_term() {
        assert!(m(&["mods"]).matches("hey mods can you help"));
    }

    #[test]
    fn punctuation_boundaries() {
        assert!(m(&["alice"]).matches("(alice)"));
        assert!(m(&["alice"]).matches("alice, hello"));
        assert!(m(&["alice"]).matches("@alice:"));
    }

    #[test]
    fn empty_matcher_never_matches() {
        assert!(!m(&[]).matches("alice"));
        assert!(!m(&["", "  ", "@"]).matches("alice"));
    }

    #[test]
    fn multiple_terms() {
        let matcher = m(&["alice", "mods"]);
        assert!(matcher.matches("hey mods"));
        assert!(matcher.matches("yo alice"));
        assert!(!matcher.matches("nothing here"));
    }
}
