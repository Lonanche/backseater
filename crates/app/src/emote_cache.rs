//! Disk cache of the native Twitch emote sets (the personal cross-channel set
//! and the viewed channel's own set), so autocomplete and the picker are
//! populated instantly at launch from the previous session while the fresh
//! Helix fetch runs — the fetch result then replaces the sets on screen and
//! re-persists them. One JSON file (`<config>/backseater/emote_cache.json`)
//! holds the sets per viewed channel, tied to the account that fetched them:
//! an account switch invalidates the whole file (the personal set is
//! per-account entitlements, never show one account's under another).

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::controller::TwitchEmotes;

const STORE: &str = "emote_cache";

#[derive(Default, Serialize, Deserialize)]
struct CacheFile {
    /// The Twitch account the sets were fetched as.
    user_id: String,
    /// Sets keyed by viewed channel login ("" for a tab with no Twitch channel).
    channels: HashMap<String, TwitchEmotes>,
}

/// Serializes the shared file's read-modify-write across concurrent tab
/// fetches (each tab saves its own channel's entry as its fetch lands).
static LOCK: Mutex<()> = Mutex::new(());

/// The cached sets for `channel` as previously fetched by `user_id`, if any.
pub fn load(user_id: &str, channel: &str) -> Option<TwitchEmotes> {
    let _guard = LOCK.lock().unwrap();
    let mut file: CacheFile = bks_auth::store::load(STORE).ok()??;
    if file.user_id != user_id {
        return None;
    }
    file.channels.remove(channel)
}

/// Persists `channel`'s freshly fetched sets, discarding the file first if it
/// belongs to a different account. Failure only costs the next warm start.
pub fn save(user_id: &str, channel: &str, emotes: &TwitchEmotes) {
    let _guard = LOCK.lock().unwrap();
    let mut file: CacheFile = bks_auth::store::load(STORE)
        .ok()
        .flatten()
        .unwrap_or_default();
    if file.user_id != user_id {
        file = CacheFile {
            user_id: user_id.to_string(),
            channels: HashMap::new(),
        };
    }
    file.channels.insert(channel.to_string(), emotes.clone());
    if let Err(e) = bks_auth::store::save(STORE, &file) {
        tracing::debug!("saving the emote cache failed: {e:#}");
    }
}
