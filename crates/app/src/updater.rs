//! Velopack auto-update: install/update hooks at startup, a background check
//! against the project's GitHub Releases, and the restart-to-apply step the
//! update banner triggers. Only active when the app runs from a Velopack
//! install — a `cargo run` / portable copy quietly reports "not installed"
//! and the updater stays off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager, VelopackApp};

/// Where releases are published; the updater reads the same feed `vpk upload
/// github` writes (see `.github/workflows/release.yml`).
const REPO_URL: &str = "https://github.com/Lonanche/backseater";
/// Re-check cadence after the launch check, while no update has been found.
pub(crate) const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

/// The downloaded-and-ready update, stashed so the banner's Restart button can
/// apply it without re-checking (and so a failed apply can be retried).
static READY: Mutex<Option<UpdateInfo>> = Mutex::new(None);

/// Whether checks also consider GitHub pre-releases (the beta channel). Set
/// from the persisted "Get beta updates" setting (`Settings.beta_updates`).
static BETA: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_beta_updates(on: bool) {
    BETA.store(on, Ordering::Relaxed);
}

/// Set on the first launch after an update applied (the Velopack restarted
/// hook) — drives the one-time "Updated to X — what's new" banner.
static UPDATED_TO: OnceLock<String> = OnceLock::new();

/// Velopack's startup hook: runs install/uninstall/update callbacks and may
/// exit or restart the process (e.g. mid-update). Must be the first thing in
/// `main`, before any other state exists.
pub(crate) fn startup() {
    VelopackApp::build()
        .on_restarted(|version| {
            let _ = UPDATED_TO.set(version.to_string());
        })
        // Uninstall leaves no junk: the image cache lives outside the install
        // root (see `bks_auth::store::image_cache_dir`), so the uninstaller
        // wouldn't remove it by itself. Hook must be fast; a dir delete is.
        .on_before_uninstall_fast_callback(|_| bks_auth::store::purge_image_cache())
        .run();
}

/// The version this launch was just updated to, if it is the first run after
/// an update applied.
pub(crate) fn just_updated_to() -> Option<String> {
    UPDATED_TO.get().cloned()
}

/// The GitHub release page for `version` — the "what's new" link target.
pub(crate) fn release_url(version: &str) -> String {
    format!("{REPO_URL}/releases/tag/v{version}")
}

/// The project repository (the Help section's links).
pub(crate) fn repo_url() -> &'static str {
    REPO_URL
}

fn manager() -> Result<UpdateManager, velopack::Error> {
    let prerelease = BETA.load(Ordering::Relaxed);
    UpdateManager::new(GithubSource::new(REPO_URL, None, prerelease), None, None)
}

/// A human label for the running build: "v0.2.1", "v0.3.0-beta.1 (beta)", or
/// "v0.2.1 (dev)" when not running from a Velopack install. The installed
/// package version is authoritative — it carries the pre-release suffix that
/// identifies a beta build; the crate version is the dev/portable fallback.
pub(crate) fn version_label() -> &'static str {
    static LABEL: OnceLock<String> = OnceLock::new();
    LABEL.get_or_init(|| match manager() {
        Ok(m) => {
            let v = m.get_current_version_as_string();
            if v.contains('-') {
                format!("v{v} (beta)")
            } else {
                format!("v{v}")
            }
        }
        Err(_) => concat!("v", env!("CARGO_PKG_VERSION"), " (dev)").to_string(),
    })
}

/// Checks GitHub Releases for a newer build and downloads it, returning its
/// version once it's ready to apply. Blocking (network + disk) — call from a
/// background thread, never the UI thread.
pub(crate) fn check_and_download() -> Option<String> {
    let manager = match manager() {
        Ok(m) => m,
        // Not a Velopack install (dev build / portable) — normal, updater off.
        Err(e) => {
            tracing::debug!("updater inactive: {e}");
            return None;
        }
    };
    let update = match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(update)) => *update,
        Ok(_) => return None,
        Err(e) => {
            tracing::warn!("update check failed: {e}");
            return None;
        }
    };
    let version = update.TargetFullRelease.Version.clone();
    tracing::info!("update {version} available, downloading");
    if let Err(e) = manager.download_updates(&update, None) {
        tracing::warn!("update download failed: {e}");
        return None;
    }
    tracing::info!("update {version} ready to apply");
    *READY.lock().unwrap() = Some(update);
    Some(version)
}

/// Applies the downloaded update and restarts the app (exits this process).
/// On failure the update is kept so the banner's button can retry.
pub(crate) fn restart_to_update() {
    let Some(update) = READY.lock().unwrap().take() else {
        return;
    };
    let result = manager().and_then(|m| m.apply_updates_and_restart(&update));
    if let Err(e) = result {
        tracing::error!("failed to apply update: {e}");
        *READY.lock().unwrap() = Some(update);
    }
}
