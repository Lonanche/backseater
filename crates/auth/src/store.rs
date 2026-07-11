//! App-data persistence: JSON files in the OS config dir
//! (`<config>/backseater/<name>.json`), generic over the value type + filename,
//! used for non-secret app data (tabs, settings). **Credentials** go through the
//! `*_secret` variants instead, which on Windows store the JSON in the OS keyring
//! (Credential Manager, DPAPI-encrypted at rest — protects tokens in config-dir
//! backups/synced folders/shared zips, though not against same-user malware; the
//! entries are visible under Generic Credentials / `cmdkey /list`). If the
//! keyring errors, the plaintext file path is the fallback so login still works
//! (a successful save then moves the credentials into the keyring and deletes
//! the file). Non-Windows platforms keep the plaintext file until they're
//! ported (matches the app's Windows-first support).

use std::path::PathBuf;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// The keyring service name credentials are filed under.
#[cfg(windows)]
const KEYRING_SERVICE: &str = "backseater";

fn path(name: &str) -> anyhow::Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("no OS config dir")?
        .join("backseater");
    Ok(dir.join(format!("{name}.json")))
}

/// The on-disk image cache directory (`<cache>/backseater-cache/images`),
/// created if needed. Lives in the OS *cache* dir (not config) since it's
/// regenerable — safe for the OS to clear. Used by the app's persistent
/// emote-image cache so emotes survive restarts without re-downloading.
/// ⚠️ NOT `<cache>/backseater`: on Windows the cache dir is `%LocalAppData%`,
/// so that path is (case-insensitively) the Velopack install root
/// (`%LocalAppData%\Backseater`) — anything kept there is wiped by
/// uninstall/repair and clutters the install directory.
pub fn image_cache_dir() -> anyhow::Result<PathBuf> {
    let dir = dirs::cache_dir()
        .context("no OS cache dir")?
        .join("backseater-cache")
        .join("images");
    std::fs::create_dir_all(&dir).context("creating image cache dir")?;
    Ok(dir)
}

/// Deletes the whole image-cache tree (`<cache>/backseater-cache`). Called by
/// the app's uninstall hook so removing the app leaves no cache behind (the
/// cache lives outside the install root, so the uninstaller alone won't).
pub fn purge_image_cache() {
    if let Some(dir) = dirs::cache_dir().map(|d| d.join("backseater-cache")) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Saves `creds` to `<config>/backseater/<name>.json`, creating the dir if needed.
pub fn save<T: Serialize>(name: &str, creds: &T) -> anyhow::Result<()> {
    let path = path(name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating config dir")?;
    }
    let json = serde_json::to_string_pretty(creds)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Loads saved credentials, or `None` if there are none yet.
pub fn load<T: DeserializeOwned>(name: &str) -> anyhow::Result<Option<T>> {
    let path = path(name)?;
    match std::fs::read_to_string(&path) {
        Ok(json) => Ok(Some(
            serde_json::from_str(&json).context("parsing saved credentials")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Removes saved credentials (logout). Succeeds if there were none.
pub fn clear(name: &str) -> anyhow::Result<()> {
    let path = path(name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// The keyring entry for `name`, or `None` (with a warning) if the keyring
/// itself is unavailable — callers then fall back to the plaintext file.
/// The target is set explicitly to `backseater.<name>` — it's what Credential
/// Manager displays, and the crate's default (`<name>.backseater`) reads
/// backwards there.
#[cfg(windows)]
fn keyring_entry(name: &str) -> Option<keyring::Entry> {
    let target = format!("{KEYRING_SERVICE}.{name}");
    match keyring::Entry::new_with_target(&target, KEYRING_SERVICE, name) {
        Ok(entry) => Some(entry),
        Err(e) => {
            tracing::warn!("OS keyring unavailable for {name}: {e}");
            None
        }
    }
}

/// Saves a credential: into the OS keyring where supported (removing any legacy
/// plaintext file), else the plaintext file.
pub fn save_secret<T: Serialize>(name: &str, value: &T) -> anyhow::Result<()> {
    #[cfg(windows)]
    if let Some(entry) = keyring_entry(name) {
        let json = serde_json::to_string(value)?;
        match entry.set_password(&json) {
            Ok(()) => {
                // A stale plaintext copy would shadow nothing (keyring is read
                // first) but shouldn't linger on disk.
                let _ = clear(name);
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("keyring save failed for {name}, using the file instead: {e}");
            }
        }
    }
    save(name, value)
}

/// Loads a credential: from the OS keyring where supported, else the plaintext
/// file. A file that predates the keyring (or was written while it was broken)
/// still loads via the fallback; the next successful [`save_secret`] puts the
/// credentials in the keyring and deletes it.
pub fn load_secret<T: DeserializeOwned>(name: &str) -> anyhow::Result<Option<T>> {
    #[cfg(windows)]
    if let Some(entry) = keyring_entry(name) {
        match entry.get_password() {
            Ok(json) => {
                return Ok(Some(
                    serde_json::from_str(&json).context("parsing saved credentials")?,
                ))
            }
            Err(keyring::Error::NoEntry) => {}
            Err(e) => {
                tracing::warn!("keyring read failed for {name}, using the file instead: {e}");
            }
        }
    }
    load(name)
}

/// Removes a credential from the keyring and any plaintext file (logout).
/// Succeeds if there was nothing stored.
pub fn clear_secret(name: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    if let Some(entry) = keyring_entry(name) {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => tracing::warn!("keyring delete failed for {name}: {e}"),
        }
    }
    clear(name)
}
