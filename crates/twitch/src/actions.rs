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

    /// The emotes the logged-in user can use, for the emote picker.
    pub async fn user_emotes(&self) -> anyhow::Result<Vec<bks_core::Emote>> {
        self.helix.user_emotes().await
    }

    /// A channel's own native emotes (sub/follower/bits) by channel login, for
    /// the picker — includes emotes the user can't use (shown like web). Resolves
    /// the login to a broadcaster id first.
    pub async fn channel_emotes(&self, channel: &str) -> anyhow::Result<Vec<bks_core::Emote>> {
        let broadcaster_id = self.helix.user_id(channel).await?;
        self.helix.channel_emotes(&broadcaster_id).await
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

    /// Allows (`true`) or denies a message AutoMod is holding for review, by the
    /// id the EventSub hold notification carried.
    pub async fn automod_message(&self, message_id: &str, allow: bool) -> anyhow::Result<()> {
        self.helix.manage_automod_message(message_id, allow).await
    }
}
