use std::sync::Arc;

use bks_core::{Author, Badge, Emote, EmoteTooltip, Message, MessageElement, Platform};
use bks_platform::{ChatEvent, EventDetails, EventKind};
use chrono::{DateTime, Utc};
use piratetok_live_rs::structs::proto::{
    messages::{WebcastChatMessage, WebcastGiftMessage},
    messages_ext::WebcastEmoteChatMessage,
    types::{CommonMessageData, EmoteData, Image},
    user::UserIdentity,
};

pub(crate) fn timestamp(common: Option<&CommonMessageData>) -> DateTime<Utc> {
    common
        .filter(|c| c.create_time > 0)
        .and_then(|c| {
            if c.create_time > 10_000_000_000 {
                DateTime::from_timestamp_millis(c.create_time)
            } else {
                DateTime::from_timestamp(c.create_time, 0)
            }
        })
        .unwrap_or_else(Utc::now)
}

fn image_url(image: &Image) -> Option<&str> {
    image
        .url_list
        .iter()
        .find(|u| u.starts_with("https://"))
        .map(String::as_str)
}

fn author(user: &UserIdentity) -> Author {
    let username = bks_core::normalize_username(&user.unique_id);
    let login = if username.is_empty() {
        format!("#{}", user.user_id)
    } else {
        username.to_lowercase()
    };
    let mut badges = Vec::new();
    for badge in &user.badge_list {
        let image = badge
            .image_badge
            .as_ref()
            .and_then(|b| b.image.as_ref())
            .or_else(|| badge.combine_badge.as_ref().and_then(|b| b.icon.as_ref()));
        let (id, title) = match badge.badge_scene {
            1 => ("moderator", "Moderator"),
            4 | 7 => ("subscriber", "Subscriber"),
            8 => ("gifter", "Gifter level"),
            10 => ("fan", "Fan club"),
            12 => ("broadcaster", "Broadcaster"),
            _ => ("tiktok", "TikTok badge"),
        };
        if let Some(url) = image.and_then(image_url) {
            badges.push(Badge {
                id: id.into(),
                url: url.into(),
                title: Some(title.into()),
            });
        }
    }
    for image in &user.badge_image_list {
        if let Some(url) = image_url(image).filter(|url| !badges.iter().any(|b| b.url == *url)) {
            badges.push(Badge {
                id: "tiktok".into(),
                url: url.into(),
                title: image.content.as_ref().map(|c| c.name.clone()),
            });
        }
    }
    Author {
        display_name: if user.nickname.is_empty() {
            login.clone()
        } else {
            user.nickname.clone()
        },
        login,
        user_id: user.user_id.to_string(),
        badges,
        ..Author::default()
    }
}

fn emote(data: &EmoteData) -> Option<MessageElement> {
    let details = data.emote.as_ref()?;
    let image = details.image.as_ref()?;
    let name = image
        .content
        .as_ref()
        .map(|c| c.name.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!(":tiktok_{}:", details.emote_id));
    Some(MessageElement::Emote(Arc::new(Emote {
        id: format!("tiktok:{}", details.emote_id),
        name,
        url: image_url(image)?.to_string(),
        animated: image.is_animated,
        tooltip: EmoteTooltip::provider("TikTok"),
    })))
}

fn message(
    channel: &str,
    common: Option<&CommonMessageData>,
    user: Option<&UserIdentity>,
    text: &str,
    emotes: &[EmoteData],
    historical: bool,
) -> Option<Message> {
    let common = common.filter(|c| c.msg_id > 0)?;
    let user = user?;
    let mut elements = Vec::new();
    if !text.is_empty() {
        elements.push(MessageElement::Text {
            text: text.to_string(),
            color: None,
        });
    }
    for data in emotes {
        if let Some(token) = emote(data) {
            if !elements.is_empty() {
                elements.push(MessageElement::Text {
                    text: " ".into(),
                    color: None,
                });
            }
            elements.push(token);
        }
    }
    if elements.is_empty() {
        return None;
    }
    let raw_text = elements
        .iter()
        .map(|e| match e {
            MessageElement::Text { text, .. } => text.as_str(),
            MessageElement::Emote(e) => &e.name,
            _ => "",
        })
        .collect::<String>();
    Some(Message {
        id: common.msg_id.to_string(),
        platform: Platform::TikTok,
        channel: channel.into(),
        timestamp: timestamp(Some(common)),
        author: author(user),
        elements: bks_core::mentionize(bks_core::linkify(elements)),
        raw_text,
        reply: None,
        first_message: false,
        highlighted: false,
        historical,
        reward_id: None,
    })
}

pub(crate) fn chat(channel: &str, msg: &WebcastChatMessage, historical: bool) -> Option<Message> {
    message(
        channel,
        msg.common.as_ref(),
        msg.user.as_ref(),
        &msg.comment,
        &msg.emotes,
        historical,
    )
}

pub(crate) fn emote_chat(
    channel: &str,
    msg: &WebcastEmoteChatMessage,
    historical: bool,
) -> Option<Message> {
    message(
        channel,
        msg.common.as_ref(),
        msg.user.as_ref(),
        "",
        &msg.emote_list,
        historical,
    )
}

pub(crate) fn event(
    kind: EventKind,
    user: Option<&UserIdentity>,
    common: Option<&CommonMessageData>,
    action: String,
) -> ChatEvent {
    let actor = user
        .map(|u| {
            if u.nickname.is_empty() {
                u.unique_id.clone()
            } else {
                u.nickname.clone()
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "A viewer".into());
    ChatEvent::Event {
        platform: Platform::TikTok,
        kind,
        text: format!("{actor} {action}"),
        timestamp: timestamp(common),
        message: None,
        details: EventDetails {
            actor: Some(actor),
            compact: Some(action),
            ..EventDetails::default()
        },
    }
}

pub(crate) fn gift(msg: &WebcastGiftMessage) -> Option<ChatEvent> {
    // Combo updates carry cumulative counts; announce the completed streak once.
    if !msg.is_streak_over() {
        return None;
    }
    let count = msg.repeat_count.max(1);
    let name = msg
        .gift_details
        .as_ref()
        .map(|g| g.gift_name.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("gift");
    Some(event(
        EventKind::Gift,
        msg.user.as_ref(),
        msg.common.as_ref(),
        format!("sent {count} × {name}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use piratetok_live_rs::structs::proto::{
        gift_types::GiftDetails,
        types::{BadgeStruct, EmoteDetails, ImageBadge},
    };

    #[test]
    fn unicode_identity_links_badges_and_history_survive_conversion() {
        let input = WebcastChatMessage {
            common: Some(CommonMessageData {
                msg_id: 42,
                create_time: 1_800_000_000,
                ..Default::default()
            }),
            user: Some(UserIdentity {
                user_id: 7,
                unique_id: "@SomeUser".into(),
                nickname: "ボブ 🎵".into(),
                badge_list: vec![BadgeStruct {
                    badge_scene: 1,
                    image_badge: Some(ImageBadge {
                        image: Some(Image {
                            url_list: vec!["https://example.com/mod.png".into()],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            comment: "こんにちは @friend https://example.com".into(),
            ..Default::default()
        };
        let msg = chat("creator", &input, true).unwrap();
        assert_eq!(msg.author.login, "someuser");
        assert_eq!(msg.author.display_name, "ボブ 🎵");
        assert_eq!(msg.author.badges[0].id, "moderator");
        assert_eq!(msg.timestamp.timestamp(), 1_800_000_000);
        assert!(msg.historical);
        assert!(msg
            .elements
            .iter()
            .any(|e| matches!(e, MessageElement::Link { .. })));
        assert!(msg
            .elements
            .iter()
            .any(|e| matches!(e, MessageElement::Mention { .. })));
        assert_eq!(msg.raw_text, input.comment);
    }

    #[test]
    fn native_emote_only_messages_are_renderable_and_searchable() {
        let input = WebcastEmoteChatMessage {
            common: Some(CommonMessageData {
                msg_id: 43,
                ..Default::default()
            }),
            user: Some(UserIdentity {
                user_id: 7,
                ..Default::default()
            }),
            emote_list: vec![EmoteData {
                emote: Some(EmoteDetails {
                    emote_id: "123".into(),
                    image: Some(Image {
                        url_list: vec!["https://example.com/emote.webp".into()],
                        is_animated: true,
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let msg = emote_chat("creator", &input, false).unwrap();
        assert_eq!(msg.author.display_name, "#7");
        assert_eq!(msg.raw_text, ":tiktok_123:");
        assert!(matches!(&msg.elements[0], MessageElement::Emote(e) if e.animated));
        assert!(chat("creator", &WebcastChatMessage::default(), false).is_none());
    }

    #[test]
    fn combo_gifts_only_announce_the_final_total() {
        let mut msg = WebcastGiftMessage {
            repeat_count: 5,
            gift_details: Some(GiftDetails {
                gift_type: 1,
                gift_name: "Rose".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(gift(&msg).is_none());
        msg.repeat_end = 1;
        assert!(
            matches!(gift(&msg), Some(ChatEvent::Event { text, .. }) if text.contains("5 × Rose"))
        );
    }
}
