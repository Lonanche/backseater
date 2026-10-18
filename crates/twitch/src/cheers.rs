//! Cheermotes are separate from native emotes: IRC supplies only a Bits total.
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use bks_core::{Color, Emote, EmoteTooltip, Message, MessageElement};
use bks_platform::{ChatEvent, EventDetails, EventKind};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
struct Cheermote {
    prefix: String,
    tiers: Vec<Tier>,
}

#[derive(Clone, Deserialize)]
struct Tier {
    min_bits: u64,
    color: String,
    images: HashMap<String, HashMap<String, HashMap<String, String>>>,
}

fn cache() -> &'static RwLock<HashMap<String, Vec<Cheermote>>> {
    static CACHE: OnceLock<RwLock<HashMap<String, Vec<Cheermote>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Load global and channel-specific artwork once on channel join/reconnect.
pub async fn load_cheermotes(
    auth: &crate::EventsubAuth,
    channel: &str,
    id: &str,
) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Cheermote>,
    }
    let response: Response = crate::http::client()
        .get("https://api.twitch.tv/helix/bits/cheermotes")
        .query(&[("broadcaster_id", id)])
        .header("Client-Id", &auth.client_id)
        .bearer_auth(&auth.token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    cache()
        .write()
        .unwrap()
        .insert(channel.to_lowercase(), response.data);
    Ok(())
}

/// Only an authoritative, positive IRC Bits tag makes a message a cheer.
pub(crate) fn privmsg_event(message: Message, bits: Option<&str>) -> ChatEvent {
    let Some(bits) = bits.and_then(|s| s.parse::<u64>().ok()).filter(|n| *n > 0) else {
        return ChatEvent::Message(Box::new(message));
    };
    let compact = format!("cheered {} Bits", bks_core::format_count(bits));
    ChatEvent::Event {
        platform: message.platform,
        kind: EventKind::Bits,
        text: format!("{} {compact}", message.author.display_name),
        timestamp: message.timestamp,
        details: EventDetails {
            bits: Some(bits),
            actor: Some(message.author.display_name.clone()),
            compact: Some(compact),
            historical: message.historical,
            ..Default::default()
        },
        message: Some(Box::new(message)),
    }
}

/// Called only for confirmed cheer events, before third-party emote resolution.
pub fn resolve_cheermotes(message: &mut Message) {
    let cache = cache().read().unwrap();
    let definitions = cache
        .get(&message.channel.to_lowercase())
        .map(Vec::as_slice)
        .unwrap_or_default();
    message.elements = resolve_elements(std::mem::take(&mut message.elements), definitions);
}

fn resolve_elements(
    elements: Vec<MessageElement>,
    definitions: &[Cheermote],
) -> Vec<MessageElement> {
    let mut result = Vec::new();
    for element in elements {
        let MessageElement::Text { text, color } = element else {
            result.push(element);
            continue;
        };
        for token in text.split_inclusive(char::is_whitespace) {
            let word = token.trim_end_matches(char::is_whitespace);
            let split = word.trim_end_matches(|c: char| c.is_ascii_digit()).len();
            let (prefix, amount) = word.split_at(split);
            let cheer = amount.parse::<u64>().ok().filter(|n| *n > 0).and_then(|amount| {
                if let Some(definition) = definitions.iter().find(|d| d.prefix.eq_ignore_ascii_case(prefix)) {
                    let tier = definition.tiers.iter().filter(|t| t.min_bits <= amount).max_by_key(|t| t.min_bits)?;
                    let images = tier.images.get("dark")?;
                    let (url, animated) = images.get("animated").and_then(|s| s.get("2")).map(|url| (url.clone(), true))
                        .or_else(|| images.get("static").and_then(|s| s.get("2")).map(|url| (url.clone(), false)))?;
                    Some((url, animated, Color::from_hex(&tier.color)))
                } else if prefix.eq_ignore_ascii_case("cheer") {
                    // Standard Cheer artwork also works for anonymous readers.
                    let (tier, color) = [(10000, "#F43021"), (5000, "#0099FE"), (1000, "#1DB2A5"), (100, "#9C3EE8"), (1, "#979797")]
                        .into_iter().find(|(min, _)| amount >= *min).unwrap();
                    Some((format!("https://d3aqoihi2n8ty8.cloudfront.net/actions/cheer/dark/animated/{tier}/2.gif"), true, Color::from_hex(color)))
                } else { None }
            });
            if let Some((url, animated, tier_color)) = cheer {
                result.push(MessageElement::Emote(Arc::new(Emote {
                    id: format!("cheermote:{}", word.to_ascii_lowercase()),
                    name: prefix.to_string(),
                    url,
                    animated,
                    tooltip: EmoteTooltip::provider("Twitch Bits"),
                })));
                result.push(MessageElement::Text {
                    text: format!("{amount}{}", &token[word.len()..]),
                    color: tier_color,
                });
            } else {
                result.push(MessageElement::Text {
                    text: token.to_string(),
                    color,
                });
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_tiers_preserve_text_and_ignore_invalid_tokens() {
        let input = "日本語 Cheer1 cheer99 CHEER100 Cheer500 Cheer1000 Cheer5000 Cheer10000 Cheer0 Cheer500! unknown500";
        let result = resolve_elements(
            vec![MessageElement::Text {
                text: input.into(),
                color: None,
            }],
            &[],
        );
        let mut copy = String::new();
        let mut urls = Vec::new();
        for element in &result {
            match element {
                MessageElement::Text { text, .. } => copy.push_str(text),
                MessageElement::Emote(e) => {
                    copy.push_str(&e.name);
                    assert!(e.animated);
                    urls.push(&e.url);
                }
                _ => panic!("unexpected element"),
            }
        }
        assert_eq!(copy, input);
        assert_eq!(urls.len(), 7);
        for (url, tier) in urls.iter().zip([1, 1, 100, 100, 1000, 5000, 10000]) {
            assert!(url.ends_with(&format!("/{tier}/2.gif")));
        }
    }

    #[test]
    fn custom_api_artwork_uses_highest_matching_tier_and_static_fallback() {
        let definitions: Vec<Cheermote> = serde_json::from_value(serde_json::json!([{
            "prefix": "Custom", "tiers": [
                { "min_bits": 1, "color": "#979797", "images": { "dark": { "animated": { "2": "https://example.com/low.gif" } } } },
                { "min_bits": 500, "color": "#FF0000", "images": { "dark": { "static": { "2": "https://example.com/high.png" } } } }
            ]
        }])).unwrap();
        let result = resolve_elements(
            vec![MessageElement::Text {
                text: "custom499 Custom500".into(),
                color: None,
            }],
            &definitions,
        );
        let images: Vec<_> = result
            .iter()
            .filter_map(|e| match e {
                MessageElement::Emote(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].url, "https://example.com/low.gif");
        assert!(images[0].animated);
        assert_eq!(images[1].url, "https://example.com/high.png");
        assert!(!images[1].animated);
    }
}
