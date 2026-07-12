//! Twitch moderation actions via Helix (ban/timeout/delete). These were removed
//! from IRC in Feb 2023, so they go through the REST API with the logged-in
//! user's token — see [`crate::helix`]. Requires the `moderator:manage:*` scopes.

use crate::helix::{Helix, UserInfo};
use crate::ivr::SubAge;

/// Account info for a chatter's usercard: their public Helix profile (avatar,
/// creation date) plus their follow + subscription standing in the channel from
/// IVR (a public, unauthenticated source — no moderator scope needed).
#[derive(Clone, Debug)]
pub struct TwitchUserCard {
    pub info: UserInfo,
    /// Follow date + sub tenure for the current channel. `None` if the IVR lookup
    /// failed (the rest of the card still loads).
    pub subage: Option<SubAge>,
}

/// Moderation for the logged-in Twitch user. The channel passed to each method
/// is the broadcaster login (resolved to an id by Helix).
pub struct TwitchActions {
    helix: Helix,
}

impl TwitchActions {
    /// `client_id` is the Twitch app id; `access_token` is the bare OAuth token
    /// (no `oauth:` prefix); `user_id` is the logged-in user (the moderator).
    pub fn new(client_id: String, access_token: String, user_id: String) -> Self {
        Self {
            helix: Helix::new(client_id, access_token, user_id),
        }
    }

    /// Builds a chatter's usercard: their Helix account info plus their follow +
    /// sub standing in `channel` from IVR. The IVR call is unauthenticated, so it
    /// works regardless of who's logged in; if it fails its field is just `None`
    /// and the rest of the card still loads.
    pub async fn usercard(&self, login: &str, channel: &str) -> anyhow::Result<TwitchUserCard> {
        // Independent fetches — run them concurrently so latency is the max, not the sum.
        let (info, subage) = tokio::join!(
            self.helix.user_info(login),
            crate::ivr::fetch_subage(login, channel)
        );
        Ok(TwitchUserCard {
            info: info?,
            subage: subage.ok(),
        })
    }

    /// The logged-in user's own id (keys per-account caches).
    pub fn own_user_id(&self) -> &str {
        self.helix.own_user_id()
    }

    /// Both native emote sets for the picker + autocomplete, fetched
    /// concurrently after ONE shared login→id resolve (each listing resolving
    /// its own raced the id cache and sent a duplicate `/users` request):
    /// the emotes the logged-in user can use (passing the channel guarantees
    /// its follower emotes are in the set) and the channel's own set
    /// (sub/follower/bits, including emotes the user can't use — shown locked
    /// in the picker like web). A failed resolve just omits the
    /// channel-dependent parts (the personal listing still runs).
    pub async fn native_emotes(
        &self,
        channel: Option<&str>,
    ) -> (
        anyhow::Result<Vec<bks_core::Emote>>,
        anyhow::Result<Vec<bks_core::Emote>>,
    ) {
        let broadcaster_id = match channel {
            Some(c) => self.helix.user_id(c).await.ok(),
            None => None,
        };
        tokio::join!(
            self.helix.user_emotes(broadcaster_id.as_deref()),
            async {
                match &broadcaster_id {
                    Some(id) => self.helix.channel_emotes(id).await,
                    None => Ok(Vec::new()),
                }
            }
        )
    }

    /// The users currently in `channel`'s chat (broadcaster/moderator only —
    /// Twitch exposes no viewer list to anyone else; see [`Helix::chatters`]).
    pub async fn chatters(&self, channel: &str) -> anyhow::Result<crate::helix::Chatters> {
        self.helix.chatters(channel).await
    }

    /// Grants/revokes moderator and VIP in `channel`. These need the broadcaster's
    /// own token (only the channel owner can change mods/VIPs), so they error for a
    /// non-owner moderator — the message is surfaced to the user.
    pub async fn add_moderator(&self, channel: &str, user: &str) -> anyhow::Result<()> {
        self.helix.add_moderator(channel, user).await
    }

    pub async fn remove_moderator(&self, channel: &str, user: &str) -> anyhow::Result<()> {
        self.helix.remove_moderator(channel, user).await
    }

    pub async fn add_vip(&self, channel: &str, user: &str) -> anyhow::Result<()> {
        self.helix.add_vip(channel, user).await
    }

    pub async fn remove_vip(&self, channel: &str, user: &str) -> anyhow::Result<()> {
        self.helix.remove_vip(channel, user).await
    }

    // Moderation via Helix (removed from IRC in Feb 2023). The channel is the
    // broadcaster login; Helix resolves it to an id.
    pub async fn ban(&self, channel: &str, user: &str, reason: Option<&str>) -> anyhow::Result<()> {
        self.helix.ban(channel, user, None, reason).await
    }

    pub async fn timeout(&self, channel: &str, user: &str, secs: u32) -> anyhow::Result<()> {
        self.helix.ban(channel, user, Some(secs), None).await
    }

    pub async fn unban(&self, channel: &str, user: &str) -> anyhow::Result<()> {
        self.helix.unban(channel, user).await
    }

    pub async fn delete_message(&self, channel: &str, message_id: &str) -> anyhow::Result<()> {
        self.helix.delete_message(channel, message_id).await
    }

    /// Pins a message in `channel`'s chat for `duration_secs` (Twitch clamps to
    /// 30–1800s; `None` = until the stream ends). Replaces any current mod pin.
    pub async fn pin_message(
        &self,
        channel: &str,
        message_id: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<()> {
        self.helix
            .pin_message(channel, message_id, duration_secs)
            .await
    }

    /// Unpins the currently pinned message (`message_id`) in `channel`'s chat.
    pub async fn unpin_message(&self, channel: &str, message_id: &str) -> anyhow::Result<()> {
        self.helix.unpin_message(channel, message_id).await
    }

    /// `/pin <message>` twitch.tv-style: sends `text` as the logged-in user
    /// (via Helix, which returns the real message id — IRC doesn't) and pins
    /// it for `duration_secs`.
    pub async fn send_and_pin(
        &self,
        channel: &str,
        text: &str,
        duration_secs: Option<u32>,
    ) -> anyhow::Result<()> {
        let message_id = self.helix.send_message(channel, text).await?;
        self.helix
            .pin_message(channel, &message_id, duration_secs)
            .await
    }

    /// Allows (`true`) or denies a message AutoMod is holding for review, by the
    /// id the EventSub hold notification carried.
    pub async fn automod_message(&self, message_id: &str, allow: bool) -> anyhow::Result<()> {
        self.helix.manage_automod_message(message_id, allow).await
    }

    /// Clears `channel`'s entire chat.
    pub async fn clear_chat(&self, channel: &str) -> anyhow::Result<()> {
        self.helix.clear_chat(channel).await
    }

    /// Posts an announcement (`color` = blue/green/orange/purple, `None` = the
    /// channel accent).
    pub async fn announce(
        &self,
        channel: &str,
        message: &str,
        color: Option<&str>,
    ) -> anyhow::Result<()> {
        self.helix.announce(channel, message, color).await
    }

    /// Warns `user` with `reason`; they must acknowledge it before chatting again.
    pub async fn warn(&self, channel: &str, user: &str, reason: &str) -> anyhow::Result<()> {
        self.helix.warn(channel, user, reason).await
    }

    /// Slow mode: `Some(secs)` (Twitch allows 3–120) turns it on, `None` off.
    pub async fn set_slow_mode(&self, channel: &str, secs: Option<u32>) -> anyhow::Result<()> {
        let body = match secs {
            Some(secs) => {
                serde_json::json!({ "slow_mode": true, "slow_mode_wait_time": secs })
            }
            None => serde_json::json!({ "slow_mode": false }),
        };
        self.helix.update_chat_settings(channel, body).await
    }

    /// Followers-only: `Some(minutes)` of minimum follow age (0 = any follower,
    /// Twitch caps at 3 months) turns it on, `None` off.
    pub async fn set_follower_mode(
        &self,
        channel: &str,
        minutes: Option<u32>,
    ) -> anyhow::Result<()> {
        let body = match minutes {
            Some(minutes) => {
                serde_json::json!({ "follower_mode": true, "follower_mode_duration": minutes })
            }
            None => serde_json::json!({ "follower_mode": false }),
        };
        self.helix.update_chat_settings(channel, body).await
    }

    /// Subscribers-only chat on/off.
    pub async fn set_sub_only(&self, channel: &str, on: bool) -> anyhow::Result<()> {
        self.helix
            .update_chat_settings(channel, serde_json::json!({ "subscriber_mode": on }))
            .await
    }

    /// Emote-only chat on/off.
    pub async fn set_emote_only(&self, channel: &str, on: bool) -> anyhow::Result<()> {
        self.helix
            .update_chat_settings(channel, serde_json::json!({ "emote_mode": on }))
            .await
    }

    /// Unique-chat (no duplicate messages) on/off.
    pub async fn set_unique_chat(&self, channel: &str, on: bool) -> anyhow::Result<()> {
        self.helix
            .update_chat_settings(channel, serde_json::json!({ "unique_chat_mode": on }))
            .await
    }

    /// Sends an official shoutout for `user` in `channel`.
    pub async fn shoutout(&self, channel: &str, user: &str) -> anyhow::Result<()> {
        self.helix.shoutout(channel, user).await
    }

    /// Starts a raid from `channel` to `target` (broadcaster only).
    pub async fn raid(&self, channel: &str, target: &str) -> anyhow::Result<()> {
        self.helix.raid(channel, target).await
    }

    /// Cancels `channel`'s pending raid (broadcaster only).
    pub async fn unraid(&self, channel: &str) -> anyhow::Result<()> {
        self.helix.cancel_raid(channel).await
    }
}
