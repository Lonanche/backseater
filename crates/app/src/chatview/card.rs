//! The usercard owns its inputs, account lookup, and selected chatter's history.

use std::sync::Arc;

use bks_core::{Message, Platform};
use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, FontWeight, SharedString, Subscription, Task, Window};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{h_flex, v_flex, ActiveTheme, Sizable};

use super::{ChatView, Row};
use crate::channel_store::{ChannelEvent, ChannelModel, RowChange};
use crate::image_cache::LruImageCache;
use crate::{child_window, commands, controller, render, selectable, usercard, USERCARD_MESSAGES};

const WINDOW_SIZE: gpui::Size<gpui::Pixels> = gpui::Size {
    width: px(440.),
    height: px(620.),
};
const MIN_SIZE: gpui::Size<gpui::Pixels> = gpui::Size {
    width: px(360.),
    height: px(300.),
};

fn max_timeout_secs(platform: Platform) -> u32 {
    match platform {
        Platform::Kick => 604_800,
        _ => 1_209_600,
    }
}

#[derive(Clone, Copy)]
enum Mod {
    Ban,
    Timeout(u32),
    Unban,
}

pub(super) fn open(host: Entity<ChatView>, card: usercard::UserCard, cx: &mut App) {
    let title = format!("{}'s Usercard", card.display_name);
    let existing = {
        let host = host.read(cx);
        host.usercard_window.zip(
            host.usercard_view
                .as_ref()
                .and_then(gpui::WeakEntity::upgrade),
        )
    };
    if let Some((handle, view)) = existing {
        if child_window::focus_existing(handle, Some(&title), cx) {
            let (channel, font_size) = {
                let host = host.read(cx);
                (host.channel.clone(), host.font_size)
            };
            let _ = handle.update(cx, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.bind_channel(channel, font_size, cx);
                    view.set_card(card, window, cx);
                });
            });
            return;
        }
    }
    let (parent, channel, image_cache, font_size) = {
        let host = host.read(cx);
        (
            host.parent_window,
            host.channel.clone(),
            host.image_cache.clone(),
            host.font_size,
        )
    };
    let opened = child_window::open_owned(
        &title,
        WINDOW_SIZE,
        MIN_SIZE,
        parent,
        Some("usercard"),
        move |window, cx| UserCardView::new(card, channel, image_cache, font_size, window, cx),
        cx,
    );
    match opened {
        Ok((handle, view)) => host.update(cx, |host, _| {
            host.usercard_window = Some(handle);
            host.usercard_view = Some(view.downgrade());
        }),
        Err(error) => tracing::warn!("could not open usercard: {error:#}"),
    }
}

pub(super) struct UserCardView {
    card: usercard::UserCard,
    channel: Entity<ChannelModel>,
    controller: controller::Controller,
    image_cache: Entity<LruImageCache>,
    font_size: f32,
    timeout_input: Entity<InputState>,
    warn_input: Entity<InputState>,
    timeout_error: Option<String>,
    warn_error: Option<String>,
    history: RecentMessages,
    stats_request: u64,
    stats_task: Option<Task<()>>,
    channel_subscription: Subscription,
    _subscriptions: Vec<Subscription>,
}

impl UserCardView {
    fn new(
        card: usercard::UserCard,
        channel: Entity<ChannelModel>,
        image_cache: Entity<LruImageCache>,
        font_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let timeout_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("90s, 10m, 1h30m…"));
        let warn_input = cx.new(|cx| InputState::new(window, cx).placeholder("Warning reason…"));
        let subscriptions = vec![
            cx.subscribe_in(&timeout_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_custom_timeout(window, cx);
                }
            }),
            cx.subscribe_in(&warn_input, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_usercard_warn(window, cx);
                }
            }),
        ];
        let mut this = Self {
            controller: channel.read(cx).controller.clone(),
            history: RecentMessages::new(channel.read(cx), &card),
            channel_subscription: cx.subscribe(&channel, Self::on_channel_event),
            card,
            channel,
            image_cache,
            font_size,
            timeout_input,
            warn_input,
            timeout_error: None,
            warn_error: None,
            stats_request: 0,
            stats_task: None,
            _subscriptions: subscriptions,
        };
        this.refresh_mod_link(cx);
        this.fetch_stats(cx);
        this
    }

    fn set_card(&mut self, card: usercard::UserCard, window: &mut Window, cx: &mut Context<Self>) {
        self.card = card;
        self.timeout_error = None;
        self.warn_error = None;
        self.timeout_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.warn_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.history = RecentMessages::new(self.channel.read(cx), &self.card);
        self.refresh_mod_link(cx);
        self.fetch_stats(cx);
        cx.notify();
    }

    pub(super) fn refresh(&mut self, font_size: f32, cx: &mut Context<Self>) {
        self.font_size = font_size;
        self.refresh_mod_link(cx);
        cx.notify();
    }

    fn bind_channel(
        &mut self,
        channel: Entity<ChannelModel>,
        font_size: f32,
        cx: &mut Context<Self>,
    ) -> bool {
        self.font_size = font_size;
        if self.channel.entity_id() == channel.entity_id() {
            return false;
        }
        self.channel_subscription = cx.subscribe(&channel, Self::on_channel_event);
        self.controller = channel.read(cx).controller.clone();
        self.channel = channel;
        self.history = RecentMessages::new(self.channel.read(cx), &self.card);
        self.card.is_broadcaster = false;
        self.card.is_moderator = false;
        if let Some((_, message)) = self.history.rows.last() {
            self.card.set_roles_from_badges(&message.author.badges);
        }
        self.timeout_error = None;
        self.warn_error = None;
        true
    }

    pub(super) fn reconnect(
        &mut self,
        channel: Entity<ChannelModel>,
        font_size: f32,
        cx: &mut Context<Self>,
    ) {
        if self.bind_channel(channel, font_size, cx) {
            self.refresh_mod_link(cx);
            self.fetch_stats(cx);
        }
        cx.notify();
    }

    fn refresh_mod_link(&mut self, cx: &App) {
        let channel = self.controller.twitch_channel();
        self.card.mod_viewercard_url = (self.card.platform == Platform::Twitch
            && self.channel.read(cx).can_moderate(Platform::Twitch)
            && !channel.is_empty()
            && !self.card.login.is_empty())
        .then(|| {
            format!(
                "https://www.twitch.tv/popout/{channel}/viewercard/{}",
                self.card.login
            )
        });
    }

    fn fetch_stats(&mut self, cx: &mut Context<Self>) {
        self.stats_task = None;
        self.stats_request += 1;
        match self.card.platform {
            Platform::Twitch if self.controller.has_twitch() => {
                self.card.stats = usercard::Stats::Loading;
                let (tx, rx) = smol::channel::bounded(1);
                self.controller
                    .fetch_twitch_usercard(self.card.login.clone(), tx);
                self.receive_stats(rx, usercard::Stats::Twitch, cx);
            }
            Platform::Kick if self.controller.has_kick() => {
                self.card.stats = usercard::Stats::Loading;
                let (tx, rx) = smol::channel::bounded(1);
                self.controller
                    .fetch_kick_usercard(self.card.login.clone(), tx);
                self.receive_stats(rx, usercard::Stats::Kick, cx);
            }
            Platform::Twitch | Platform::Kick => {
                self.card.stats = usercard::Stats::Unavailable(
                    "This platform is no longer part of the tab".into(),
                );
            }
            _ => {}
        }
    }

    fn receive_stats<T: 'static>(
        &mut self,
        rx: smol::channel::Receiver<anyhow::Result<T>>,
        wrap: impl FnOnce(T) -> usercard::Stats + 'static,
        cx: &mut Context<Self>,
    ) {
        let request = self.stats_request;
        let platform = self.card.platform;
        let login = self.card.login.clone();
        self.stats_task = Some(cx.spawn(async move |weak, cx| {
            if let Ok(result) = rx.recv().await {
                let _ = weak.update(cx, |this, cx| {
                    if this.stats_request != request
                        || this.card.platform != platform
                        || this.card.login != login
                    {
                        return;
                    }
                    this.card.stats = match result {
                        Ok(data) => wrap(data),
                        Err(error) => usercard::Stats::Unavailable(format!("{error:#}")),
                    };
                    cx.notify();
                });
            }
        }));
    }

    fn on_channel_event(
        &mut self,
        _: Entity<ChannelModel>,
        event: &ChannelEvent,
        cx: &mut Context<Self>,
    ) {
        if self.history.apply(event, &self.card) {
            cx.notify();
        }
        match event {
            ChannelEvent::RowsChanged(change) => {
                let affects_card = match change {
                    RowChange::All => true,
                    RowChange::Message(platform, id) => self
                        .history
                        .rows
                        .iter()
                        .any(|(_, message)| *platform == message.platform && *id == message.id),
                    RowChange::Author(platform, id) => {
                        self.history.rows.iter().any(|(_, message)| {
                            *platform == message.platform && *id == message.author.user_id
                        })
                    }
                };
                if affects_card {
                    // Retokenization can replace a message Arc; reseed against
                    // the settled model and skip any already-covered events.
                    self.history = RecentMessages::new(self.channel.read(cx), &self.card);
                    self.refresh_mod_link(cx);
                    cx.notify();
                }
            }
            ChannelEvent::Repaint => cx.notify(),
            ChannelEvent::StatusChanged => {
                self.refresh_mod_link(cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn apply_custom_timeout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.timeout_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let platform = self.card.platform;
        match bks_core::parse_duration(&text) {
            None => {
                self.timeout_error = Some(format!(
                    "Can't read \"{text}\" — try 90s, 10m, 1h30m, or 3d"
                ));
            }
            Some(secs) if secs > u64::from(max_timeout_secs(platform)) => {
                self.timeout_error = Some(match platform {
                    Platform::Kick => "Kick timeouts max out at 7 days".to_string(),
                    _ => "Twitch timeouts max out at 2 weeks".to_string(),
                });
            }
            Some(secs) => {
                self.usercard_moderate(platform, Mod::Timeout(secs as u32), &self.card.login);
                self.timeout_error = None;
                self.timeout_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
            }
        }
        cx.notify();
    }

    fn apply_usercard_warn(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.card.platform != Platform::Twitch {
            return;
        }
        let reason = self.warn_input.read(cx).value().trim().to_string();
        if reason.is_empty() {
            self.warn_error =
                Some("Enter a reason — the chatter has to acknowledge it".to_string());
        } else if reason.chars().count() > 500 {
            self.warn_error = Some("Warning reasons max out at 500 characters".to_string());
        } else {
            self.controller.warn_twitch(self.card.login.clone(), reason);
            self.warn_error = None;
            self.warn_input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }

    fn usercard_actions(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let card = &self.card;
        let platform = card.platform;
        let can_moderate = self.channel.read(cx).can_moderate(platform);
        if !can_moderate {
            return div().into_any_element();
        }
        // The target broadcaster can't be banned/timed out or granted a role at
        // all; a target moderator can't be banned/timed out until they're
        // unmodded. Hide the buttons that would always fail rather than show them
        // dead. Role grants (mod/VIP) also need a *broadcaster* token, so only the
        // channel owner sees them — a plain moderator can't add/remove mod or VIP.
        let is_broadcaster = card.is_broadcaster;
        // A Twitch login tier without the matching scopes hides the affordance
        // (it could only 401) — Kick's scope set is fixed, so only Twitch gates.
        // The scope slices come from the command registry so the gates can't
        // drift from what /ban, /mod, /vip actually require.
        let twitch = platform == bks_core::Platform::Twitch;
        let show_ban_timeout = !is_broadcaster
            && !card.is_moderator
            && !(twitch && crate::session::twitch_scope_missing(commands::SCOPE_BANNED_USERS));
        // Mod and VIP grants are gated per scope (a token could carry one and
        // not the other), matching the popup's per-command /mod and /vip gates.
        let can_grant_mod =
            !(twitch && crate::session::twitch_scope_missing(commands::SCOPE_MODERATORS));
        let can_grant_vip = !(twitch && crate::session::twitch_scope_missing(commands::SCOPE_VIPS));
        let show_roles = !is_broadcaster
            && self.channel.read(cx).twitch_broadcaster
            && (can_grant_mod || can_grant_vip);
        let login = card.login.clone();

        // The user's own custom mod buttons (Settings → Mod Buttons), filtered
        // to this card's platform (scope Both/None or a matching platform, and
        // supported on it) and to those that act on a *user* — the card has no
        // message, so "/delete"/`{msg-id}` buttons are skipped. Labeled with the
        // button's name; each runs its template against this login. These show
        // even for a mod/broadcaster target (a bot shoutout isn't a ban).
        let custom_buttons: Vec<gpui::AnyElement> = crate::settings::mod_buttons()
            .iter()
            .filter(|b| b.platform.is_none_or(|p| p == platform))
            .filter(|b| commands::supported_on(&b.command, platform))
            .filter(|b| commands::targets_user(&b.command))
            .filter(|b| {
                !twitch
                    || !crate::session::twitch_scope_missing(commands::twitch_scopes_for_template(
                        &b.command,
                    ))
            })
            .enumerate()
            .map(|(i, b)| {
                let label = if b.name.is_empty() {
                    b.command.clone()
                } else {
                    b.name.clone()
                };
                let command = b.command.clone();
                let to_login = login.clone();
                Button::new(SharedString::from(format!("usercard-custom-{i}")))
                    .label(SharedString::from(label))
                    .outline()
                    .xsmall()
                    .compact()
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.run_usercard_mod_button(&command, &to_login, platform);
                    }))
                    .into_any_element()
            })
            .collect();
        let custom_buttons_row = (!custom_buttons.is_empty()).then(|| {
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_1()
                .children(custom_buttons)
        });

        if !show_ban_timeout && !show_roles && custom_buttons_row.is_none() {
            return div().into_any_element();
        }

        // (label, seconds) timeout presets — Chatterino's spread, through the
        // full 2-week Twitch cap; presets over the platform's cap are dropped
        // (Kick tops out at 7 days).
        const PRESETS: &[(&str, u32)] = &[
            ("1s", 1),
            ("1m", 60),
            ("10m", 600),
            ("30m", 1800),
            ("1h", 3600),
            ("4h", 14400),
            ("1d", 86400),
            ("3d", 259_200),
            ("1w", 604_800),
            ("2w", 1_209_600),
        ];
        let max = max_timeout_secs(platform);

        let timeout_chips = h_flex().w_full().flex_wrap().gap_1().children(
            PRESETS
                .iter()
                .filter(|(_, secs)| *secs <= max)
                .map(|(label, secs)| {
                    let secs = *secs;
                    let to_login = login.clone();
                    Button::new(SharedString::from(format!("usercard-to-{label}")))
                        .label(*label)
                        .outline()
                        .xsmall()
                        .compact()
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.usercard_moderate(platform, Mod::Timeout(secs), &to_login);
                        }))
                }),
        );

        // The custom-duration row: a small parse-anything box ("90s", "1h30m",
        // "3d") applied by Enter or its button. The input only exists while the
        // usercard window is open (it's bound to it).
        let custom_row = h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.timeout_input).small()),
            )
            .child(
                Button::new("usercard-to-custom")
                    .label("Timeout")
                    .outline()
                    .small()
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.apply_custom_timeout(window, cx);
                    })),
            );
        let custom_error = self.timeout_error.as_ref().map(|err| {
            div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(SharedString::from(err.clone()))
        });

        // Warn (Twitch-only — Kick has no warn API): a reason box + button;
        // Helix requires the reason, and the chatter must acknowledge the
        // warning before they can chat again. Applied by Enter or the button.
        // Gated on its own warnings scope, not show_ban_timeout's banned_users
        // — a Basic-moderation login can ban but not warn.
        let warn_row = (platform == bks_core::Platform::Twitch
            && !crate::session::twitch_scope_missing(commands::SCOPE_WARNINGS))
        .then_some(&self.warn_input)
        .map(|input| {
            h_flex()
                .w_full()
                .gap_1()
                .items_center()
                .child(div().flex_1().child(Input::new(input).small()))
                .child(
                    Button::new("usercard-warn")
                        .label("Warn")
                        .outline()
                        .small()
                        .compact()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.apply_usercard_warn(window, cx);
                        })),
                )
        });
        let warn_error = self.warn_error.as_ref().map(|err| {
            div()
                .text_size(px(12.))
                .text_color(cx.theme().danger)
                .child(SharedString::from(err.clone()))
        });

        // Ban + Unban, always present for both platforms.
        let ban_login = login.clone();
        let ban = Button::new("usercard-ban")
            .label("Ban")
            .danger()
            .xsmall()
            .compact()
            .on_click(cx.listener(move |this, _, _, _| {
                this.usercard_moderate(platform, Mod::Ban, &ban_login);
            }));
        let unban_login = login.clone();
        let unban = Button::new("usercard-unban")
            .label("Unban")
            .outline()
            .xsmall()
            .compact()
            .on_click(cx.listener(move |this, _, _, _| {
                this.usercard_moderate(platform, Mod::Unban, &unban_login);
            }));

        // Role grants are Twitch-only (Kick's public API can't add/remove mod/VIP).
        // Both directions are shown as separate buttons (Mod/Unmod, VIP/Unvip) so
        // the action never depends on (possibly stale) detected role state.
        let roles = (show_roles && platform == bks_core::Platform::Twitch).then(|| {
            let role_btn = |id: &'static str,
                            label: &'static str,
                            role: controller::Role,
                            grant: bool,
                            login: SharedString| {
                Button::new(id)
                    .label(label)
                    .outline()
                    .xsmall()
                    .compact()
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.controller
                            .set_role_twitch(role, grant, login.to_string());
                    }))
            };
            let login = SharedString::from(login.clone());
            h_flex()
                .gap_1()
                .when(can_grant_mod, |row| {
                    row.child(role_btn(
                        "usercard-mod",
                        "Mod",
                        controller::Role::Moderator,
                        true,
                        login.clone(),
                    ))
                    .child(role_btn(
                        "usercard-unmod",
                        "Unmod",
                        controller::Role::Moderator,
                        false,
                        login.clone(),
                    ))
                })
                .when(can_grant_vip, |row| {
                    row.child(role_btn(
                        "usercard-vip",
                        "VIP",
                        controller::Role::Vip,
                        true,
                        login.clone(),
                    ))
                    .child(role_btn(
                        "usercard-unvip",
                        "Unvip",
                        controller::Role::Vip,
                        false,
                        login.clone(),
                    ))
                })
        });

        // A compact, sectioned panel: the timeout chips + custom-duration row,
        // a Ban/Unban row, (Twitch broadcaster only) a "Role" row, and the
        // user's custom mod buttons.
        let section_label = |text: &'static str| {
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(text))
        };
        v_flex()
            .debug_selector(|| "usercard-actions".into())
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(cx.theme().secondary)
            .when(show_ban_timeout, |col| {
                col.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(section_label("Timeout"))
                        .child(timeout_chips)
                        .child(custom_row)
                        .when_some(custom_error, |col, err| col.child(err)),
                )
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_1()
                        .child(ban)
                        .child(unban),
                )
                .when_some(warn_row, |col, row| {
                    col.child(
                        v_flex()
                            .w_full()
                            .gap_1()
                            .child(section_label("Warn"))
                            .child(row)
                            .when_some(warn_error, |col, err| col.child(err)),
                    )
                })
            })
            .when_some(roles, |col, role_row| {
                col.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(section_label("Role"))
                        .child(role_row.w_full().flex_wrap()),
                )
            })
            .when_some(custom_buttons_row, |col, row| {
                col.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .child(section_label("Custom"))
                        .child(row),
                )
            })
            .into_any_element()
    }

    /// Routes a usercard moderation action to the controller for the card's
    /// platform (Twitch or Kick). Centralizes the per-platform dispatch so the
    /// chip handlers don't each branch on the platform.
    fn usercard_moderate(&self, platform: bks_core::Platform, action: Mod, login: &str) {
        let c = &self.controller;
        let login = login.to_string();
        match (platform, action) {
            (bks_core::Platform::Twitch, Mod::Ban) => c.ban_twitch(login),
            (bks_core::Platform::Twitch, Mod::Timeout(s)) => c.timeout_twitch(login, s),
            (bks_core::Platform::Twitch, Mod::Unban) => c.unban_twitch(login),
            (bks_core::Platform::Kick, Mod::Ban) => c.ban_kick(login),
            (bks_core::Platform::Kick, Mod::Timeout(s)) => c.timeout_kick(login, s),
            (bks_core::Platform::Kick, Mod::Unban) => c.unban_kick(login),
            _ => {}
        }
    }

    fn run_usercard_mod_button(&self, command: &str, login: &str, platform: bks_core::Platform) {
        let mut template = command.to_string();
        if !command.contains("{user}") {
            let is_user_target = command
                .strip_prefix('/')
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(commands::implicit_target)
                == Some(commands::ImplicitTarget::User);
            if is_user_target {
                let mut parts = command.splitn(2, char::is_whitespace);
                let head = parts.next().unwrap_or_default();
                template = match parts.next() {
                    Some(rest) => format!("{head} {{user}} {rest}"),
                    None => format!("{head} {{user}}"),
                };
            }
        }
        let line = template.replace("{user}", login);
        self.controller.handle_input_at(&line, platform);
    }

    fn message_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let content = if self.history.rows.is_empty() {
            div()
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child("No recent messages in this channel.")
                .into_any_element()
        } else {
            let selection = selectable::Selection::new();
            selection.begin_frame();
            let mut ordinal = 0;
            let model = self.channel.read(cx);
            let rows = self.history.rows.iter().map(|(_, msg)| {
                render::render_message(
                    msg,
                    render::RowFlags {
                        cosmetics: model.cosmetics_for(msg.platform, &msg.author.user_id),
                        struck: model.is_struck(msg),
                        ..Default::default()
                    },
                    self.font_size,
                    &selection,
                    &mut ordinal,
                    render::RowHandlers::default(),
                )
                .into_any_element()
            });
            div()
                .id("usercard-messages")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .child(v_flex().gap_1().children(rows))
                .into_any_element()
        };
        v_flex()
            .flex_1()
            .min_h(px(0.))
            .gap_1()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Recent messages ({})", self.history.rows.len())),
            )
            .child(content)
            .into_any_element()
    }
}

impl Render for UserCardView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::stale_hover::clear(window, cx);
        let reveal = cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
            this.card.avatar_revealed = true;
            cx.notify();
        });
        let header = self.card.header(reveal, cx);
        let actions = self.usercard_actions(cx);
        let messages = self.message_list(cx);
        gpui::image_cache(self.image_cache.clone()).child(
            v_flex()
                .size_full()
                .p_4()
                .gap_3()
                .bg(gpui::rgb(render::panel_bg()))
                .text_color(cx.theme().foreground)
                .child(header)
                .child(actions)
                .child(messages),
        )
    }
}

#[derive(Default)]
struct RecentMessages {
    rows: Vec<(usize, Arc<Message>)>,
    generation: u64,
}

impl RecentMessages {
    fn new(model: &ChannelModel, card: &usercard::UserCard) -> Self {
        let mut rows: Vec<_> = model
            .rows
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, row)| match row {
                Row::Message { msg } if Self::matches(card, msg) => Some((index, msg.clone())),
                _ => None,
            })
            .take(USERCARD_MESSAGES)
            .collect();
        rows.reverse();
        Self {
            rows,
            generation: model.rows_generation(),
        }
    }

    fn matches(card: &usercard::UserCard, msg: &Message) -> bool {
        msg.platform == card.platform
            && msg.author.login.eq_ignore_ascii_case(&card.login)
            && !msg.historical
    }

    fn apply(&mut self, event: &ChannelEvent, card: &usercard::UserCard) -> bool {
        let generation = match event {
            ChannelEvent::Appended { generation, .. }
            | ChannelEvent::Inserted { generation, .. }
            | ChannelEvent::RemovedFront { generation } => *generation,
            _ => return false,
        };
        if generation <= self.generation {
            return false;
        }
        self.generation = generation;
        match event {
            ChannelEvent::Appended {
                index,
                msg: Some(msg),
                is_message: true,
                ..
            } if Self::matches(card, msg) => {
                self.rows.push((*index, msg.clone()));
                if self.rows.len() > USERCARD_MESSAGES {
                    self.rows.remove(0);
                }
                true
            }
            ChannelEvent::Inserted { index, .. } => {
                for (row_index, _) in &mut self.rows {
                    if *row_index >= *index {
                        *row_index += 1;
                    }
                }
                false
            }
            ChannelEvent::RemovedFront { .. } => {
                let old_len = self.rows.len();
                self.rows.retain_mut(|(index, _)| {
                    if *index == 0 {
                        false
                    } else {
                        *index -= 1;
                        true
                    }
                });
                self.rows.len() != old_len
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(platform: Platform) -> usercard::UserCard {
        usercard::UserCard::new("alice".into(), "Alice".into(), "42".into(), platform, None)
    }

    fn message(id: &str, platform: Platform, login: &str, historical: bool) -> Arc<Message> {
        Arc::new(Message {
            id: id.into(),
            platform,
            channel: "fixture".into(),
            timestamp: chrono::Utc::now(),
            author: bks_core::Author {
                login: login.into(),
                display_name: login.into(),
                user_id: "42".into(),
                color: None,
                badges: Vec::new(),
                paint: None,
            },
            raw_text: id.into(),
            elements: Vec::new(),
            reply: None,
            first_message: false,
            highlighted: false,
            historical,
            reward_id: None,
        })
    }

    fn append(index: usize, generation: u64, message: Arc<Message>) -> ChannelEvent {
        ChannelEvent::Appended {
            index,
            generation,
            historical: message.historical,
            msg: Some(message),
            is_message: true,
        }
    }

    #[test]
    fn history_filters_platform_and_backlog_and_tracks_insertions_and_trims() {
        let card = card(Platform::Twitch);
        let mut recent = RecentMessages::default();
        for (index, msg) in [
            message("first", Platform::Twitch, "ALICE", false),
            message("other-platform", Platform::Kick, "alice", false),
            message("other-user", Platform::Twitch, "bob", false),
            message("backlog", Platform::Twitch, "alice", true),
            message("last", Platform::Twitch, "alice", false),
        ]
        .into_iter()
        .enumerate()
        {
            recent.apply(&append(index, index as u64 + 1, msg), &card);
        }
        assert_eq!(
            recent
                .rows
                .iter()
                .map(|(i, m)| (*i, m.id.as_str()))
                .collect::<Vec<_>>(),
            [(0, "first"), (4, "last")]
        );
        let insertion = ChannelEvent::Inserted {
            index: 0,
            generation: 6,
            msg: Some(message("older", Platform::Twitch, "alice", true)),
            is_message: true,
        };
        assert!(!recent.apply(&insertion, &card));
        assert!(!recent.apply(&ChannelEvent::RemovedFront { generation: 7 }, &card));
        assert!(recent.apply(&ChannelEvent::RemovedFront { generation: 8 }, &card));
        assert_eq!(recent.rows.len(), 1);
        assert_eq!(recent.rows[0].0, 3);
        assert_eq!(recent.rows[0].1.id, "last");
        assert!(
            !recent.apply(&insertion, &card),
            "replayed events are ignored"
        );
        assert_eq!(recent.rows[0].0, 3);
    }

    #[test]
    fn history_keeps_only_the_latest_message_limit() {
        let card = card(Platform::Twitch);
        let mut recent = RecentMessages::default();
        for index in 0..USERCARD_MESSAGES + 3 {
            recent.apply(
                &append(
                    index,
                    index as u64 + 1,
                    message(&index.to_string(), Platform::Twitch, "alice", false),
                ),
                &card,
            );
        }
        assert_eq!(recent.rows.len(), USERCARD_MESSAGES);
        assert_eq!(recent.rows[0].0, 3);
        assert_eq!(recent.rows.last().unwrap().0, USERCARD_MESSAGES + 2);
    }

    #[gpui::test]
    fn empty_history_card_updates_moderation_controls_on_permission_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        cx.update(|cx| {
            gpui_component::init(cx);
            LruImageCache::install_for_test(cx);
        });
        let mut content = None;
        let window = cx.add_window(|window, cx| {
            let channel = crate::channel_store::register_for_test(
                crate::session::Session::for_test(),
                "fixture",
                cx,
            );
            let image_cache = LruImageCache::try_shared(cx).unwrap();
            let view = cx.new(|cx| {
                UserCardView::new(
                    card(Platform::Kick),
                    channel.clone(),
                    image_cache,
                    14.,
                    window,
                    cx,
                )
            });
            content = Some((view.clone(), channel));
            gpui_component::Root::new(view, window, cx)
        });
        let (view, channel) = content.unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        visual.update(|window, cx| {
            window.draw(cx).clear();
            assert!(view.read(cx).history.rows.is_empty());
        });
        assert!(visual.debug_bounds("usercard-actions").is_none());
        for is_mod in [true, false] {
            visual.update(|_, cx| {
                channel.update(cx, |channel, cx| {
                    channel.push(
                        bks_platform::ChatEvent::ModStatus {
                            platform: Platform::Kick,
                            is_mod,
                            is_broadcaster: false,
                        },
                        cx,
                    );
                });
            });
            visual.run_until_parked();
            visual.update(|window, cx| {
                // Draw only what the permission event invalidated.
                window.draw(cx).clear();
                assert!(view.read(cx).history.rows.is_empty());
            });
            assert_eq!(visual.debug_bounds("usercard-actions").is_some(), is_mod);
        }
    }

    #[gpui::test]
    fn replacing_same_login_on_another_platform_cancels_old_account_lookup(
        cx: &mut gpui::TestAppContext,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        cx.update(|cx| {
            gpui_component::init(cx);
            LruImageCache::install_for_test(cx);
        });
        let window = cx.add_window(|_, _| gpui::Empty);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let view = visual.update(|window, cx| {
            let channel = crate::channel_store::register_for_test(
                crate::session::Session::for_test(),
                "fixture",
                cx,
            );
            let image_cache = LruImageCache::try_shared(cx).unwrap();
            cx.new(|cx| {
                UserCardView::new(
                    card(Platform::Twitch),
                    channel,
                    image_cache,
                    14.,
                    window,
                    cx,
                )
            })
        });
        let (old_tx, old_rx) = smol::channel::bounded(1);
        let (new_tx, new_rx) = smol::channel::bounded(1);
        visual.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.receive_stats(old_rx, usercard::Stats::Unavailable, cx);
                view.timeout_error = Some("stale timeout".into());
                view.warn_error = Some("stale warning".into());
                view.timeout_input
                    .update(cx, |input, cx| input.set_value("10m", window, cx));
                view.set_card(card(Platform::Kick), window, cx);
                view.receive_stats(new_rx, usercard::Stats::Unavailable, cx);
                assert!(view.timeout_error.is_none());
                assert!(view.warn_error.is_none());
                assert!(view.timeout_input.read(cx).value().is_empty());
            });
        });
        visual.run_until_parked();
        assert!(old_tx.try_send(Ok("stale Twitch result".into())).is_err());
        new_tx.try_send(Ok("current Kick result".into())).unwrap();
        visual.run_until_parked();
        visual.update(|_, cx| {
            let card = &view.read(cx).card;
            assert_eq!(card.platform, Platform::Kick);
            assert!(matches!(&card.stats, usercard::Stats::Unavailable(text) if text == "current Kick result"));
        });
        let old_channel = visual.update(|_, cx| {
            let old_channel = view.read(cx).channel.downgrade();
            let replacement = crate::channel_store::register_for_test(
                crate::session::Session::for_test(),
                "replacement",
                cx,
            );
            view.update(cx, |view, cx| {
                view.reconnect(replacement.clone(), 18., cx);
                assert_eq!(view.channel.entity_id(), replacement.entity_id());
                assert_eq!(view.controller.twitch_channel(), "replacement");
                assert_eq!(view.font_size, 18.);
                assert!(view.history.rows.is_empty());
                assert!(matches!(&view.card.stats, usercard::Stats::Unavailable(text) if text.contains("no longer")));
            });
            old_channel
        });
        visual.run_until_parked();
        assert!(
            old_channel.upgrade().is_none(),
            "reconnect releases the old model"
        );
    }
}
