//! Turns bare URLs inside [`Text`] runs into [`Link`] elements, and `@name`
//! words into [`Mention`] elements.
//!
//! Connectors call [`linkify`] + [`mentionize`] on their element stream
//! *before* emote resolution, so the emote resolver only ever sees the
//! remaining text runs and never splits a URL. A run is scanned word-by-word
//! (whitespace separated): a word starting with `http://`, `https://`, or
//! `www.` becomes a [`Link`], with trailing punctuation peeled off so
//! `(see https://x.com).` links just the URL; a word starting with `@` becomes
//! a [`Mention`] the same way. Whether an `@name` is a *real* user is
//! unknowable from the text alone (any word is a legal username shape), so
//! every one is tokenized — the UI's usercard lookup answers it on click.
//!
//! [`Text`]: MessageElement::Text
//! [`Link`]: MessageElement::Link
//! [`Mention`]: MessageElement::Mention

use crate::message::MessageElement;

/// Punctuation commonly trailing a URL in prose; trimmed off the link (a
/// closing paren is only trimmed when the URL has no matching open paren, so
/// `https://en.wikipedia.org/wiki/Foo_(bar)` keeps its paren).
const TRAILING: &[char] = &['.', ',', '!', '?', ';', ':', '"', '\'', ')', ']', '}', '…'];

/// Whether `word` looks like a URL we should linkify.
fn is_url(word: &str) -> bool {
    word.starts_with("http://") || word.starts_with("https://") || word.starts_with("www.")
}

/// Leading punctuation that may wrap a URL in prose (e.g. `(https://x.com`).
const LEADING: &[char] = &['(', '[', '{', '"', '\'', '<'];

/// The byte length of leading punctuation before a URL starts, so `(https://…`
/// keeps its `(` as prose. Stops at the first URL-prefix character.
fn leading_len(word: &str) -> usize {
    let mut start = 0;
    for c in word.chars() {
        if !LEADING.contains(&c) {
            break;
        }
        start += c.len_utf8();
    }
    start
}

/// The byte length of the link portion of `word` (whose URL starts at byte 0),
/// peeling trailing prose punctuation. A closing paren is kept when the kept
/// URL has more `(` than `)` (Wikipedia-style links).
fn url_len(word: &str) -> usize {
    let mut end = word.len();
    while end > 0 {
        let kept = &word[..end];
        let last = kept.chars().next_back().unwrap();
        if !TRAILING.contains(&last) {
            break;
        }
        // Keep a closing paren that balances an open paren earlier in the URL;
        // compare the URL *without* this char so a balanced `(bar)` stays.
        if last == ')' {
            let without = &word[..end - last.len_utf8()];
            if without.matches('(').count() > without.matches(')').count() {
                break;
            }
        }
        end -= last.len_utf8();
    }
    end
}

/// The full URL must have something after the scheme/prefix to be a link, so a
/// bare `http://` or `www.` alone stays plain text.
fn has_host(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("www."))
        .unwrap_or(url);
    !rest.is_empty()
}

/// Rewrites the element stream, splitting each [`Text`] run's bare URLs into
/// [`Link`] elements while preserving order and the run's color. Non-text
/// elements (emotes, badges, mentions, existing links) pass through untouched.
///
/// [`Text`]: MessageElement::Text
/// [`Link`]: MessageElement::Link
pub fn linkify(elements: Vec<MessageElement>) -> Vec<MessageElement> {
    let mut out = Vec::with_capacity(elements.len());
    for element in elements {
        let MessageElement::Text { text, color } = element else {
            out.push(element);
            continue;
        };
        if !text.contains("://") && !text.contains("www.") {
            out.push(MessageElement::Text { text, color });
            continue;
        }
        split_text_run(&text, color, &mut out);
    }
    out
}

/// Walks one text run, emitting alternating `Text` and `Link` elements. Spans
/// of whitespace and non-URL words are coalesced into single `Text` runs so we
/// don't fragment ordinary prose.
fn split_text_run(text: &str, color: Option<crate::message::Color>, out: &mut Vec<MessageElement>) {
    let mut pending = String::new();
    let flush = |pending: &mut String, out: &mut Vec<MessageElement>| {
        if !pending.is_empty() {
            out.push(MessageElement::Text {
                text: std::mem::take(pending),
                color,
            });
        }
    };

    // Iterate over words while preserving the exact whitespace between them.
    let mut rest = text;
    while !rest.is_empty() {
        let ws_end = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        if ws_end > 0 {
            pending.push_str(&rest[..ws_end]);
            rest = &rest[ws_end..];
            continue;
        }
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        rest = &rest[word_end..];

        // Peel leading wrappers like `(` so `(https://x.com)` still links.
        let lead = leading_len(word);
        let candidate = &word[lead..];
        if is_url(candidate) {
            let len = url_len(candidate);
            let url = &candidate[..len];
            if len > 0 && has_host(url) {
                pending.push_str(&word[..lead]); // leading punctuation as prose
                flush(&mut pending, out);
                out.push(MessageElement::Link {
                    url: url.to_string(),
                    text: url.to_string(),
                });
                pending.push_str(&candidate[len..]); // trailing punctuation as prose
                continue;
            }
        }
        pending.push_str(word);
    }
    flush(&mut pending, out);
}

/// Characters that can appear in a chat username (letters, digits, underscore —
/// the same word shape [`crate::MentionMatcher`] splits on).
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Rewrites the element stream, splitting each [`Text`] run's `@name` words
/// into [`Mention`] elements while preserving order and the run's color.
/// Non-text elements pass through untouched. A mention must *start* its word
/// (after leading wrappers like `(`), so `email@x.com` stays plain text;
/// trailing punctuation is peeled off as prose, so `@alice,` mentions `alice`.
///
/// [`Text`]: MessageElement::Text
/// [`Mention`]: MessageElement::Mention
pub fn mentionize(elements: Vec<MessageElement>) -> Vec<MessageElement> {
    let mut out = Vec::with_capacity(elements.len());
    for element in elements {
        let MessageElement::Text { text, color } = element else {
            out.push(element);
            continue;
        };
        if !text.contains('@') {
            out.push(MessageElement::Text { text, color });
            continue;
        }
        split_mention_run(&text, color, &mut out);
    }
    out
}

/// Walks one text run, emitting alternating `Text` and `Mention` elements —
/// the same whitespace-preserving word walk as [`split_text_run`].
fn split_mention_run(
    text: &str,
    color: Option<crate::message::Color>,
    out: &mut Vec<MessageElement>,
) {
    let mut pending = String::new();
    let flush = |pending: &mut String, out: &mut Vec<MessageElement>| {
        if !pending.is_empty() {
            out.push(MessageElement::Text {
                text: std::mem::take(pending),
                color,
            });
        }
    };

    let mut rest = text;
    while !rest.is_empty() {
        let ws_end = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        if ws_end > 0 {
            pending.push_str(&rest[..ws_end]);
            rest = &rest[ws_end..];
            continue;
        }
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        rest = &rest[word_end..];

        // Peel leading wrappers like `(` so `(@alice)` still mentions.
        let lead = leading_len(word);
        if let Some(name) = word[lead..].strip_prefix('@') {
            let name_len: usize = name
                .chars()
                .take_while(|&c| is_name_char(c))
                .map(char::len_utf8)
                .sum();
            if name_len > 0 {
                pending.push_str(&word[..lead]); // leading punctuation as prose
                flush(&mut pending, out);
                out.push(MessageElement::Mention {
                    login: name[..name_len].to_string(),
                });
                pending.push_str(&name[name_len..]); // trailing punctuation as prose
                continue;
            }
        }
        pending.push_str(word);
    }
    flush(&mut pending, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Color, MessageElement};

    fn text(s: &str) -> MessageElement {
        MessageElement::Text {
            text: s.into(),
            color: None,
        }
    }

    fn describe(els: &[MessageElement]) -> Vec<String> {
        els.iter()
            .map(|e| match e {
                MessageElement::Text { text, .. } => format!("T:{text}"),
                MessageElement::Link { url, .. } => format!("L:{url}"),
                MessageElement::Emote(em) => format!("E:{}", em.name),
                MessageElement::Mention { login } => format!("M:{login}"),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn no_url_is_unchanged() {
        let out = linkify(vec![text("hello world")]);
        assert_eq!(describe(&out), vec!["T:hello world"]);
    }

    #[test]
    fn url_mid_sentence() {
        let out = linkify(vec![text("see https://x.com now")]);
        assert_eq!(describe(&out), vec!["T:see ", "L:https://x.com", "T: now"]);
    }

    #[test]
    fn trailing_punctuation_is_prose() {
        let out = linkify(vec![text("go to https://x.com.")]);
        assert_eq!(describe(&out), vec!["T:go to ", "L:https://x.com", "T:."]);
    }

    #[test]
    fn paren_wrapped_url_keeps_matching_paren() {
        let out = linkify(vec![text("(https://x.com)")]);
        assert_eq!(describe(&out), vec!["T:(", "L:https://x.com", "T:)"]);
    }

    #[test]
    fn wikipedia_style_paren_kept() {
        let out = linkify(vec![text("https://en.wikipedia.org/wiki/Foo_(bar)")]);
        assert_eq!(
            describe(&out),
            vec!["L:https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
    }

    #[test]
    fn multiple_urls() {
        let out = linkify(vec![text("a http://one.com b www.two.com c")]);
        assert_eq!(
            describe(&out),
            vec!["T:a ", "L:http://one.com", "T: b ", "L:www.two.com", "T: c"]
        );
    }

    #[test]
    fn bare_scheme_is_not_a_link() {
        let out = linkify(vec![text("https:// nope")]);
        assert_eq!(describe(&out), vec!["T:https:// nope"]);
    }

    #[test]
    fn preserves_color_and_non_text_elements() {
        let red = Some(Color::rgb(255, 0, 0));
        let out = linkify(vec![
            MessageElement::Text {
                text: "look https://x.com".into(),
                color: red,
            },
            MessageElement::Mention {
                login: "bob".into(),
            },
        ]);
        match &out[0] {
            MessageElement::Text { color, .. } => assert_eq!(*color, red),
            _ => panic!("expected text first"),
        }
        assert_eq!(describe(&out), vec!["T:look ", "L:https://x.com", "M:bob"]);
    }

    #[test]
    fn mention_mid_sentence() {
        let out = mentionize(vec![text("hey @alice how are you")]);
        assert_eq!(describe(&out), vec!["T:hey ", "M:alice", "T: how are you"]);
    }

    #[test]
    fn mention_trailing_punctuation_is_prose() {
        let out = mentionize(vec![text("@alice, hi")]);
        assert_eq!(describe(&out), vec!["M:alice", "T:, hi"]);
    }

    #[test]
    fn mention_wrapped_in_parens() {
        let out = mentionize(vec![text("(@alice)")]);
        assert_eq!(describe(&out), vec!["T:(", "M:alice", "T:)"]);
    }

    #[test]
    fn email_is_not_a_mention() {
        let out = mentionize(vec![text("mail me at bob@example.com")]);
        assert_eq!(describe(&out), vec!["T:mail me at bob@example.com"]);
    }

    #[test]
    fn bare_at_is_not_a_mention() {
        let out = mentionize(vec![text("just @ nothing @@ here")]);
        assert_eq!(describe(&out), vec!["T:just @ nothing @@ here"]);
    }

    #[test]
    fn mention_keeps_typed_case_and_underscores() {
        let out = mentionize(vec![text("yo @Some_User42!")]);
        assert_eq!(describe(&out), vec!["T:yo ", "M:Some_User42", "T:!"]);
    }

    #[test]
    fn multiple_mentions() {
        let out = mentionize(vec![text("@a and @b")]);
        assert_eq!(describe(&out), vec!["M:a", "T: and ", "M:b"]);
    }

    #[test]
    fn mention_preserves_color_and_non_text_elements() {
        let red = Some(Color::rgb(255, 0, 0));
        let out = mentionize(vec![
            MessageElement::Text {
                text: "hi @bob".into(),
                color: red,
            },
            MessageElement::Link {
                url: "https://x.com".into(),
                text: "https://x.com".into(),
            },
        ]);
        match &out[0] {
            MessageElement::Text { color, .. } => assert_eq!(*color, red),
            _ => panic!("expected text first"),
        }
        assert_eq!(describe(&out), vec!["T:hi ", "M:bob", "L:https://x.com"]);
    }
}
