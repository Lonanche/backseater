//! The chatter "usercard": its own OS window opened by clicking a name in chat,
//! showing the account's info and that person's recent messages in this channel.
//!
//! State lives on the [`ChatView`](crate::ChatView) (it owns the messages and the
//! hosting child window); this module holds the card's own data + the body
//! renderer. Account stats load asynchronously (Twitch Helix), so the header
//! shows a loading/failed state until they arrive.

use bks_core::Platform;
use bks_kick::KickUserInfo;
use bks_twitch::{SubAge, TwitchUserCard};
use gpui::prelude::*;
use gpui::{div, img, px, rgb, FontWeight, SharedString};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{h_flex, v_flex, ActiveTheme, Sizable};

/// Async-loaded account stats for the card header.
pub enum Stats {
    /// Fetch in flight.
    Loading,
    /// Twitch account info loaded.
    Twitch(TwitchUserCard),
    /// Kick channel-relationship info loaded (follow date, sub months, mod flag,
    /// avatar — from the per-channel user endpoint).
    Kick(KickUserInfo),
    /// Couldn't load (not logged in, lookup failed, or not a Twitch user). The
    /// string is shown muted in the header.
    Unavailable(String),
}

/// One open usercard. Identifies the target and carries whatever account stats
/// have loaded; the past-message list is pulled from the live feed at render time.
pub struct UserCard {
    /// Lowercased login of the target — used to filter their messages and as the
    /// moderation target.
    pub login: String,
    /// Display name shown in the header (proper case).
    pub display_name: String,
    /// Numeric id from the message that opened the card (shown as "ID: …").
    pub user_id: String,
    /// The platform the clicked message came from (only Twitch loads stats today).
    pub platform: Platform,
    /// Chosen name color (packed RGB), for the header name.
    pub color: Option<u32>,
    pub stats: Stats,
    /// The target is the channel broadcaster (owner). Detected from the badges on
    /// the message that opened the card; hides Ban/Timeout and Mod/VIP (you can't
    /// moderate the owner).
    pub is_broadcaster: bool,
    /// The target is a moderator of this channel (from the opening message's
    /// badges). Hides Ban/Timeout (a mod can't be banned/timed out without first
    /// being unmodded).
    pub is_moderator: bool,
    /// While streamer mode is active the avatar renders as a placeholder;
    /// clicking it sets this to show the real image (per card, not persisted).
    pub avatar_revealed: bool,
}

impl UserCard {
    pub fn new(
        login: String,
        display_name: String,
        user_id: String,
        platform: Platform,
        color: Option<u32>,
    ) -> Self {
        Self {
            login,
            display_name,
            user_id,
            platform,
            color,
            is_broadcaster: false,
            is_moderator: false,
            // Twitch + Kick both load account info asynchronously; other
            // platforms have no lookup yet.
            stats: match platform {
                Platform::Twitch | Platform::Kick => Stats::Loading,
                _ => Stats::Unavailable("Account details aren't available yet".into()),
            },
            avatar_revealed: false,
        }
    }

    /// The account's profile/channel URL on its platform, for the header's "open
    /// profile" link. Twitch and Kick key on the `login` (their URL slug);
    /// YouTube has no public login slug in chat, so it uses the `UC…` channel id
    /// carried in `user_id`. `None` when the needed identifier is empty or the
    /// platform has no web profile.
    pub fn profile_url(&self) -> Option<String> {
        match self.platform {
            Platform::Twitch if !self.login.is_empty() => {
                Some(format!("https://twitch.tv/{}", self.login))
            }
            Platform::Kick if !self.login.is_empty() => {
                Some(format!("https://kick.com/{}", self.login))
            }
            Platform::YouTube if !self.user_id.is_empty() => {
                Some(format!("https://www.youtube.com/channel/{}", self.user_id))
            }
            _ => None,
        }
    }

    /// Records the target's channel role from the badges on the message that
    /// opened the card. Twitch badge ids are `set-id/version` (e.g. `moderator/1`);
    /// Kick uses the bare type (`moderator`) — matching the set-id prefix covers
    /// both. The broadcaster and moderator flags gate which mod buttons show.
    pub fn set_roles_from_badges(&mut self, badges: &[bks_core::Badge]) {
        for b in badges {
            match b.id.split('/').next().unwrap_or("") {
                "broadcaster" => self.is_broadcaster = true,
                "moderator" => self.is_moderator = true,
                _ => {}
            }
        }
    }

    /// The account-info header: avatar, name, id, and the loaded stat lines (or a
    /// loading / unavailable note while they aren't ready). `on_reveal` is the
    /// click handler for the streamer-mode avatar placeholder (the card's host
    /// owns the state, so it flips `avatar_revealed` there).
    pub fn header(
        &self,
        on_reveal: impl Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static,
        cx: &mut gpui::App,
    ) -> gpui::AnyElement {
        let name_color = self.color.unwrap_or(0x9147ff);

        let mut lines = v_flex().gap_1();
        // Name row: the colored display name, the platform's logo, and — when the
        // platform has a web profile — a link that opens it in the browser.
        let mut name_row = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(18.))
                    .text_color(rgb(name_color))
                    .child(SharedString::from(self.display_name.clone())),
            )
            .child(crate::platform_icon(self.platform, 14.));
        if let Some(url) = self.profile_url() {
            name_row = name_row.child(
                Button::new("usercard-open-profile")
                    .label("Open profile ↗")
                    .ghost()
                    .xsmall()
                    .compact()
                    .on_click(move |_, _, cx| {
                        cx.open_url(&url);
                    }),
            );
        }
        lines = lines.child(name_row);
        // Identity line: the login when it differs from the display name
        // (localized Twitch names), and the numeric id with a copy button.
        {
            let mut parts = Vec::new();
            if !self.login.is_empty() && !self.login.eq_ignore_ascii_case(&self.display_name) {
                parts.push(self.login.clone());
            }
            if !self.user_id.is_empty() {
                parts.push(format!("ID: {}", self.user_id));
            }
            if !parts.is_empty() {
                let mut row = h_flex()
                    .gap_1()
                    .items_center()
                    .child(stat_line(cx, &parts.join(" · ")));
                if !self.user_id.is_empty() {
                    let id = self.user_id.clone();
                    row = row.child(
                        Button::new("usercard-copy-id")
                            .label("⧉")
                            .ghost()
                            .xsmall()
                            .compact()
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(id.clone()));
                            }),
                    );
                }
                lines = lines.child(row);
            }
        }

        let avatar_url = match &self.stats {
            Stats::Twitch(card) if !card.info.profile_image_url.is_empty() => {
                Some(card.info.profile_image_url.clone())
            }
            Stats::Kick(info) if !info.profile_pic.is_empty() => Some(info.profile_pic.clone()),
            _ => None,
        };

        match &self.stats {
            Stats::Loading => {
                lines = lines.child(stat_line(cx, "loading account details…"));
            }
            Stats::Unavailable(why) => {
                lines = lines.child(stat_line(cx, why));
            }
            Stats::Twitch(card) => {
                if let Some(created) = friendly_date(&card.info.created_at) {
                    lines = lines.child(stat_line(cx, &format!("Created: {created}")));
                }
                // Follow + sub standing come from IVR; absent if that lookup failed.
                if let Some(sub) = &card.subage {
                    match sub.following_since.as_deref().and_then(friendly_date) {
                        Some(since) => {
                            lines =
                                lines.child(stat_line(cx, &format!("❤ Following since {since}")));
                        }
                        None => {
                            lines = lines.child(stat_line(cx, "Not following this channel"));
                        }
                    }
                    lines = lines.child(stat_line(cx, &sub_line(sub)));
                }
            }
            Stats::Kick(info) => {
                // The per-channel endpoint gives this chatter's standing in *this*
                // channel: follow date and sub months (like Twitch), plus mod flag.
                match info.following_since.as_deref().and_then(friendly_date) {
                    Some(since) => {
                        lines = lines.child(stat_line(cx, &format!("❤ Following since {since}")));
                    }
                    None => {
                        lines = lines.child(stat_line(cx, "Not following this channel"));
                    }
                }
                lines = lines.child(stat_line(cx, &kick_sub_line(info.subscribed_for)));
                if info.is_moderator {
                    lines = lines.child(stat_line(cx, "★ Moderator"));
                }
            }
        }

        // Reserve a fixed avatar slot (placeholder until the image loads) and a
        // minimum header height for the fully-loaded line count, so the card
        // doesn't resize/reflow when async stats arrive — that was the "jumpy"
        // pop-in. The lines area justifies to the top within that space.
        // Streamer mode blanks the slot (avatars can be personal); a click on the
        // placeholder reveals this card's avatar.
        let avatar = if crate::streamer_mode::is_active() && !self.avatar_revealed {
            div()
                .id("usercard-avatar-hidden")
                .size(px(AVATAR_SIZE))
                .rounded_md()
                .bg(cx.theme().secondary)
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().muted))
                .text_color(cx.theme().muted_foreground)
                .child(
                    v_flex()
                        .items_center()
                        .child(div().text_size(px(16.)).child(SharedString::from("🕶")))
                        .child(div().text_size(px(9.)).child(SharedString::from("hidden")))
                        .child(
                            div()
                                .text_size(px(9.))
                                .child(SharedString::from("click to show")),
                        ),
                )
                .on_mouse_down(gpui::MouseButton::Left, on_reveal)
                .into_any_element()
        } else {
            div()
                .size(px(AVATAR_SIZE))
                .rounded_md()
                .bg(cx.theme().secondary)
                .flex_shrink_0()
                .when_some(avatar_url, |slot, url| {
                    slot.child(
                        img(SharedString::from(url))
                            .id("usercard-avatar")
                            .size(px(AVATAR_SIZE))
                            .rounded_md(),
                    )
                })
                .into_any_element()
        };

        h_flex()
            .gap_3()
            .items_start()
            .min_h(px(HEADER_MIN_HEIGHT))
            .child(avatar)
            .child(lines)
            .into_any_element()
    }
}

/// Minimum header height (px), sized to the fully-loaded stat-line count so the
/// header keeps a constant height from the loading state through to loaded.
const HEADER_MIN_HEIGHT: f32 = 112.0;

/// Avatar edge length (px) in the header.
const AVATAR_SIZE: f32 = 64.0;

/// A muted single-line stat in the header.
fn stat_line(cx: &mut gpui::App, text: &str) -> gpui::AnyElement {
    div()
        .text_size(px(13.))
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

/// The subscription-status line: currently subbed
/// shows tier + cumulative months; a hidden status or a lapsed-but-formerly-subbed
/// state get their own phrasings; never-subbed is "Not subscribed".
fn sub_line(sub: &SubAge) -> String {
    if sub.status_hidden {
        return "Subscription status hidden".to_string();
    }
    if sub.subscribed {
        let tier = sub.tier.as_deref().unwrap_or("1");
        let months = sub.total_months.max(1);
        let unit = bks_core::plural(months, "month", "months");
        return format!("★ Tier {tier} - Subscribed for {months} {unit}");
    }
    if sub.total_months > 0 {
        let unit = bks_core::plural(sub.total_months, "month", "months");
        return format!("Previously subscribed for {} {unit}", sub.total_months);
    }
    "Not subscribed".to_string()
}

/// Kick's subscription line for the usercard: months subscribed to this channel,
/// or "Not subscribed" when zero. Kick exposes only a month count (no tier).
fn kick_sub_line(months: u64) -> String {
    if months == 0 {
        return "Not subscribed".to_string();
    }
    let unit = bks_core::plural(months, "month", "months");
    format!("★ Subscribed for {months} {unit}")
}

/// Formats an RFC-3339 timestamp (as Helix returns) into a plain `YYYY-MM-DD`
/// date. Returns `None` for an empty/unparseable string so the line is omitted.
fn friendly_date(rfc3339: &str) -> Option<String> {
    bks_core::parse_rfc3339(rfc3339).map(|dt| dt.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::{friendly_date, kick_sub_line, sub_line, UserCard};
    use bks_core::{Badge, Platform};
    use bks_twitch::SubAge;

    fn badge(id: &str) -> Badge {
        Badge {
            id: id.into(),
            url: String::new(),
            title: None,
        }
    }

    fn card() -> UserCard {
        UserCard::new(
            "user".into(),
            "User".into(),
            "1".into(),
            Platform::Twitch,
            None,
        )
    }

    #[test]
    fn detects_twitch_versioned_badges() {
        let mut c = card();
        c.set_roles_from_badges(&[badge("subscriber/12"), badge("moderator/1")]);
        assert!(c.is_moderator);
        assert!(!c.is_broadcaster);

        let mut c = card();
        c.set_roles_from_badges(&[badge("broadcaster/1")]);
        assert!(c.is_broadcaster);
    }

    #[test]
    fn detects_kick_bare_badges() {
        let mut c = card();
        c.set_roles_from_badges(&[badge("moderator")]);
        assert!(c.is_moderator);

        let mut c = card();
        c.set_roles_from_badges(&[badge("broadcaster")]);
        assert!(c.is_broadcaster);
    }

    #[test]
    fn plain_viewer_has_no_role() {
        let mut c = card();
        c.set_roles_from_badges(&[badge("subscriber/3"), badge("vip/1")]);
        assert!(!c.is_moderator);
        assert!(!c.is_broadcaster);
    }

    #[test]
    fn kick_sub_line_months_and_none() {
        assert_eq!(kick_sub_line(0), "Not subscribed");
        assert_eq!(kick_sub_line(1), "★ Subscribed for 1 month");
        assert_eq!(kick_sub_line(3), "★ Subscribed for 3 months");
    }

    #[test]
    fn sub_line_current() {
        let s = SubAge {
            subscribed: true,
            tier: Some("1".into()),
            total_months: 54,
            ..Default::default()
        };
        assert_eq!(sub_line(&s), "★ Tier 1 - Subscribed for 54 months");
    }

    #[test]
    fn sub_line_previous_and_hidden_and_none() {
        let prev = SubAge {
            total_months: 3,
            ..Default::default()
        };
        assert_eq!(sub_line(&prev), "Previously subscribed for 3 months");

        let hidden = SubAge {
            status_hidden: true,
            ..Default::default()
        };
        assert_eq!(sub_line(&hidden), "Subscription status hidden");

        assert_eq!(sub_line(&SubAge::default()), "Not subscribed");
    }

    #[test]
    fn profile_url_per_platform() {
        let twitch = UserCard::new("oilrats".into(), "OilRats".into(), "1".into(), Platform::Twitch, None);
        assert_eq!(twitch.profile_url().as_deref(), Some("https://twitch.tv/oilrats"));

        let kick = UserCard::new("qaixx".into(), "Qaixx".into(), "2".into(), Platform::Kick, None);
        assert_eq!(kick.profile_url().as_deref(), Some("https://kick.com/qaixx"));

        // YouTube uses the UC… channel id (no login slug in chat).
        let yt = UserCard::new(String::new(), "Creator".into(), "UC123".into(), Platform::YouTube, None);
        assert_eq!(
            yt.profile_url().as_deref(),
            Some("https://www.youtube.com/channel/UC123")
        );

        // Missing identifier → no link.
        let bare = UserCard::new(String::new(), "Anon".into(), String::new(), Platform::Twitch, None);
        assert_eq!(bare.profile_url(), None);
    }

    #[test]
    fn formats_rfc3339_to_date() {
        assert_eq!(
            friendly_date("2017-11-18T00:00:00Z").as_deref(),
            Some("2017-11-18")
        );
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(friendly_date(""), None);
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(friendly_date("not a date"), None);
    }
}
