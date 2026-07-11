mod bttv;
mod ffz;
mod http;
mod paints;
mod registry;
mod seventv;
mod seventv_api;

pub use bttv::BttvProvider;
pub use ffz::FfzProvider;
pub use paints::{
    enabled as paints_enabled, resolve as resolve_cosmetics, set_enabled as set_paints_enabled,
    Cosmetics,
};
pub use registry::{EmoteMap, EmoteRegistry};
pub use seventv::SeventvProvider;
pub use seventv_api::fetch_emote;

use async_trait::async_trait;
use bks_core::Emote;

/// A source of chat emotes (7TV, BTTV, FFZ, ...). Implementors fetch a global
/// set and a per-channel set; [`EmoteRegistry`] merges several providers and
/// resolves words against the result. Adding a provider is implementing this
/// trait and pushing it into the bridge's provider list — nothing else changes.
#[async_trait]
pub trait EmoteProvider: Send + Sync {
    /// Short provider name for logs/UI, e.g. `"7TV"`.
    fn name(&self) -> &'static str;

    /// Emotes available in every channel.
    async fn load_global(&self) -> anyhow::Result<Vec<Emote>>;

    /// Emotes specific to one channel, addressed by the platform's channel id.
    async fn load_channel(&self, channel_id: &str) -> anyhow::Result<Vec<Emote>>;
}
