//! Whole-word, case-insensitive matching of a message against a set of mention
//! terms (your account name, plus user-defined terms like "mods").
//!
//! A term matches when it appears as a standalone word in the message text — so
//! "alice" matches `alice`, `@alice`, and `Alice`, but not `aliceX`. A
//! leading `@` on either side is ignored. This is pure (no GUI/state) so the UI
//! can build a matcher from the current login names + settings and test it.

/// One compiled term: the lowercase text plus whether a match on it should
/// play the mention alert sound (per-term mute in the UI).
#[derive(Clone)]
struct Term {
    text: String,
    sound: bool,
}

/// A compiled set of lowercase mention terms. Cheap to build; clone freely.
#[derive(Clone, Default)]
pub struct MentionMatcher {
    terms: Vec<Term>,
}

/// Normalizes a raw term the way the matcher stores it: trimmed, leading `@`
/// removed, lowercased. Public so the UI's muted-term list can key on the same
/// form the matcher compares against.
pub fn normalize_term(term: &str) -> String {
    term.trim().trim_start_matches('@').to_lowercase()
}

impl MentionMatcher {
    /// Builds a matcher from raw terms (all with sound enabled). Each term is
    /// normalized ([`normalize_term`]); blank terms are dropped. Duplicates are
    /// harmless.
    pub fn new(terms: impl IntoIterator<Item = String>) -> Self {
        Self::with_sound(terms.into_iter().map(|t| (t, true)))
    }

    /// Builds a matcher from `(term, sound)` pairs — `sound: false` marks a term
    /// whose matches highlight but stay silent (muted in the UI).
    pub fn with_sound(terms: impl IntoIterator<Item = (String, bool)>) -> Self {
        let terms = terms
            .into_iter()
            .map(|(t, sound)| Term {
                text: normalize_term(&t),
                sound,
            })
            .filter(|t| !t.text.is_empty())
            .collect();
        Self { terms }
    }

    /// Whether any term appears as a standalone word in `text`.
    pub fn matches(&self, text: &str) -> bool {
        self.match_terms(text).next().is_some()
    }

    /// Whether a match in `text` should play the alert sound: true when any
    /// *matching* term has sound enabled (a muted term still highlights).
    pub fn sound_for(&self, text: &str) -> bool {
        self.match_terms(text).any(|t| t.sound)
    }

    /// All terms that appear as standalone words in `text`. Allocation-free:
    /// this runs per visible chat row per repaint (the log tints mentions at
    /// render), so the word split is re-walked lazily per term instead of
    /// collected into a `Vec` — with the typical one or two terms that's
    /// strictly cheaper. `@` is not a word char, so `@name` already splits to
    /// `name` with no trimming; empty split segments never equal a (non-empty)
    /// term.
    fn match_terms<'a>(&'a self, text: &'a str) -> impl Iterator<Item = &'a Term> + 'a {
        self.terms.iter().filter(move |term| {
            text.split(|c: char| !is_word_char(c))
                .any(|word| word.eq_ignore_ascii_case(&term.text))
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
    fn sound_follows_the_matching_terms_flags() {
        let m = MentionMatcher::with_sound(vec![
            ("alice".to_string(), false),
            ("mods".to_string(), true),
        ]);
        // A muted term still highlights — just silently.
        assert!(m.matches("hi alice"));
        assert!(!m.sound_for("hi alice"));
        assert!(m.sound_for("hey mods"));
        // Any matching term with sound on wins.
        assert!(m.sound_for("alice ping the mods"));
        assert!(!m.sound_for("nothing here"));
    }

    #[test]
    fn normalize_matches_the_matcher_form() {
        assert_eq!(super::normalize_term(" @Alice "), "alice");
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
