//! Converts a Twitch PRIVMSG into renderable [`MessageElement`]s.
//!
//! Twitch's native `emotes` tag looks like `25:0-4,12-16/1902:6-10`: an emote
//! id followed by inclusive, code-point-based ranges into the message text. We
//! split the text on those ranges into alternating [`Text`] and [`Emote`]
//! tokens. This is M1's only emote source — no external service needed.

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
    let chars: Vec<char> = text.chars().collect();
    let ranges = parse_emote_ranges(raw_emotes);

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

    for (start, end, id) in ranges {
        // Skip malformed/overlapping ranges defensively.
        if start > end || start >= chars.len() || start < cursor {
            continue;
        }
        let end = end.min(chars.len() - 1);
        push_text(&mut elements, &chars[cursor..start]);
        let name: String = chars[start..=end].iter().collect();
        elements.push(MessageElement::Emote(std::sync::Arc::new(Emote {
            url: emote_url(&id),
            id,
            name,
            animated: false,
            tooltip: bks_core::EmoteTooltip::provider("Twitch"),
        })));
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
}
