//! Velopack auto-update: install/update hooks at startup, a background check
//! against the project's GitHub Releases, and the restart-to-apply step the
//! update banner triggers. Only active when the app runs from a Velopack
//! install — a `cargo run` / portable copy quietly reports "not installed"
//! and the updater stays off.

use std::sync::Mutex;
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

/// Velopack's startup hook: runs install/uninstall/update callbacks and may
/// exit or restart the process (e.g. mid-update). Must be the first thing in
/// `main`, before any other state exists.
pub(crate) fn startup() {
    VelopackApp::build().run();
}

fn manager() -> Result<UpdateManager, velopack::Error> {
    UpdateManager::new(GithubSource::new(REPO_URL, None, false), None, None)
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
