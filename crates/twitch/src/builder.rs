//! Converts a Twitch PRIVMSG into renderable [`MessageElement`]s.
//!
//! Twitch's native `emotes` tag looks like `25:0-4,12-16/1902:6-10`: an emote
//! id followed by inclusive, code-point-based ranges into the message text. We
//! split the text on those ranges into text and image tokens. The `gifs` tag
//! supplies additional `start-end|id|url` ranges for subscriber GIF messages.

use bks_core::{Color, Emote, MessageElement};

/// `static-cdn` URL for a Twitch emote at 2x in the dark theme (also used by the
/// pubsub points/emote path).
pub(crate) fn emote_url(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/default/dark/2.0")
}

/// Parses the raw `emotes` tag into `(start, end_inclusive, id)` ranges.
fn parse_emote_ranges(raw_emotes: &str) -> Vec<(usize, usize, String)> {
    let mut ranges = Vec::new();
    if raw_emotes.is_empty() {
        return ranges;
    }
    for group in raw_emotes.split('/') {
        let Some((id, positions)) = group.split_once(':') else {
            continue;
        };
        for span in positions.split(',') {
            let Some((start, end)) = span.split_once('-') else {
                continue;
            };
            if let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) {
                ranges.push((start, end, id.to_string()));
            }
        }
    }
    ranges.sort_by_key(|(start, _, _)| *start);
    ranges
}

/// Splits message `text` into text runs and emotes using the raw `emotes` tag.
/// `text_color` is applied to every text run (Twitch message bodies have no
/// per-run color, so this is the whole message's color or `None`).
pub fn build_privmsg_elements(
    text: &str,
    raw_emotes: &str,
    text_color: Option<Color>,
) -> Vec<MessageElement> {
    build_privmsg_elements_with_gifs(text, raw_emotes, "", text_color)
}

pub(crate) fn gif_element(id: &str, url: &str, text: &str) -> Option<MessageElement> {
    let parsed = reqwest::Url::parse(url).ok()?;
    if id.is_empty() || parsed.scheme() != "https" || parsed.host_str().is_none() {
        return None;
    }
    Some(MessageElement::Gif {
        id: id.to_string(),
        // Twitch requires the complete supplied URL, including query parameters.
        url: url.to_string(),
        text: text.to_string(),
    })
}

pub(crate) fn build_privmsg_elements_with_gifs(
    text: &str,
    raw_emotes: &str,
    raw_gifs: &str,
    text_color: Option<Color>,
) -> Vec<MessageElement> {
    let chars: Vec<char> = text.chars().collect();
    let mut ranges = Vec::new();
    for gif in raw_gifs.split(',') {
        let mut parts = gif.splitn(3, '|');
        let Some((start, end)) = parts.next().and_then(|span| span.split_once('-')) else {
            continue;
        };
        let (Ok(start), Ok(end), Some(id), Some(url)) = (
            start.parse::<usize>(),
            end.parse::<usize>(),
            parts.next(),
            parts.next(),
        ) else {
            continue;
        };
        let url = tmi::maybe_unescape(url);
        if let Some(element) = gif_element(id, &url, "") {
            ranges.push((start, end, element));
        }
    }
    ranges.extend(
        parse_emote_ranges(raw_emotes)
            .into_iter()
            .map(|(start, end, id)| {
                (
                    start,
                    end,
                    MessageElement::Emote(std::sync::Arc::new(Emote {
                        url: emote_url(&id),
                        id,
                        name: String::new(),
                        animated: false,
                        tooltip: bks_core::EmoteTooltip::provider("Twitch"),
                    })),
                )
            }),
    );
    ranges.sort_by_key(|(start, _, _)| *start);

    let mut elements = Vec::new();
    let mut cursor = 0usize;

    let push_text = |elements: &mut Vec<MessageElement>, slice: &[char]| {
        if !slice.is_empty() {
            elements.push(MessageElement::Text {
                text: slice.iter().collect(),
                color: text_color,
            });
        }
    };

    for (start, end, mut element) in ranges {
        // Skip malformed/overlapping ranges defensively.
        if start > end || start >= chars.len() || start < cursor {
            continue;
        }
        let end = end.min(chars.len() - 1);
        push_text(&mut elements, &chars[cursor..start]);
        let name: String = chars[start..=end].iter().collect();
        match &mut element {
            MessageElement::Emote(emote) => std::sync::Arc::make_mut(emote).name = name,
            MessageElement::Gif { text, .. } => *text = name,
            _ => unreachable!(),
        }
        elements.push(element);
        cursor = end + 1;
    }
    push_text(&mut elements, &chars[cursor.min(chars.len())..]);

    bks_core::mentionize(bks_core::linkify(elements))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts_and_emotes(elements: &[MessageElement]) -> Vec<String> {
        elements
            .iter()
            .map(|e| match e {
                MessageElement::Text { text, .. } => format!("T:{text}"),
                MessageElement::Emote(em) => format!("E:{}", em.name),
                MessageElement::Gif { text, .. } => format!("G:{text}"),
                _ => "?".into(),
            })
            .collect()
    }

    #[test]
    fn no_emotes_is_single_text_run() {
        let els = build_privmsg_elements("hello world", "", None);
        assert_eq!(texts_and_emotes(&els), vec!["T:hello world"]);
    }

    #[test]
    fn splits_text_around_a_single_emote() {
        // "Kappa test" with Kappa (id 25) at positions 0-4.
        let els = build_privmsg_elements("Kappa test", "25:0-4", None);
        assert_eq!(texts_and_emotes(&els), vec!["E:Kappa", "T: test"]);
        match &els[0] {
            MessageElement::Emote(e) => {
                assert_eq!(e.id, "25");
                assert!(e.url.contains("/emoticons/v2/25/"));
            }
            _ => panic!("expected emote first"),
        }
    }

    #[test]
    fn handles_multiple_emotes_and_repeats() {
        // Real-world shape: two emote ids, several ranges each, unordered.
        let text = "Kappa Keepo Kappa";
        let raw = "1902:6-10/25:0-4,12-16";
        let els = build_privmsg_elements(text, raw, None);
        assert_eq!(
            texts_and_emotes(&els),
            vec!["E:Kappa", "T: ", "E:Keepo", "T: ", "E:Kappa"]
        );
    }

    #[test]
    fn gifs_and_emotes_use_codepoint_ranges_in_message_order() {
        let elements = build_privmsg_elements_with_gifs(
            "😀 é [Hi] Kappa [Bye] end",
            "25:9-13",
            "15-19|bye|https://media.giphy.com/bye.gif,4-7|hi|https://media.giphy.com/hi.gif",
            Some(Color::rgb(1, 2, 3)),
        );
        assert_eq!(
            texts_and_emotes(&elements),
            [
                "T:😀 é ",
                "G:[Hi]",
                "T: ",
                "E:Kappa",
                "T: ",
                "G:[Bye]",
                "T: end",
            ]
        );
        assert!(
            matches!(&elements[0], MessageElement::Text { color: Some(c), .. }
            if *c == Color::rgb(1, 2, 3))
        );
    }

    #[test]
    fn gif_url_is_preserved_after_irc_unescaping() {
        let url = "https://media4.giphy.com/media/abc/giphy.gif?cid=one%2Ftwo&ep=v1_gifs_trending&rid=giphy.gif&ct=g;extra=1";
        let tag = format!("0-3|abc|{}", url.replace(';', "\\:"));
        let elements = build_privmsg_elements_with_gifs("[Hi]", "", &tag, None);
        assert!(
            matches!(&elements[..], [MessageElement::Gif { id, url: actual, text }]
            if id == "abc" && actual == url && text == "[Hi]")
        );
    }

    #[test]
    fn malformed_gifs_leave_the_caption_as_text() {
        for tag in [
            "",
            "broken",
            "0-3|id",
            "x-3|id|https://media.giphy.com/a.gif",
            "3-0|id|https://media.giphy.com/a.gif",
            "99-100|id|https://media.giphy.com/a.gif",
            "0-3||https://media.giphy.com/a.gif",
            "0-3|id|",
            "0-3|id|file:///private.gif",
            "0-3|id|poster://https://media.giphy.com/a.gif",
        ] {
            let elements = build_privmsg_elements_with_gifs("[Hi]", "", tag, None);
            assert_eq!(texts_and_emotes(&elements), ["T:[Hi]"], "{tag}");
        }
        assert!(build_privmsg_elements_with_gifs(
            "",
            "",
            "0-3|id|https://media.giphy.com/a.gif",
            None,
        )
        .is_empty());
    }

    #[test]
    fn overlapping_ranges_do_not_duplicate_gif_captions() {
        let elements = build_privmsg_elements_with_gifs(
            "[Hi] end",
            "25:0-3",
            "0-3|hi|https://media.giphy.com/hi.gif,1-3|overlap|https://media.giphy.com/other.gif",
            None,
        );
        assert_eq!(texts_and_emotes(&elements), ["G:[Hi]", "T: end"]);
    }
}
