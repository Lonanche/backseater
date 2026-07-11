//! Streamer mode: hides privacy-sensitive UI (usercard avatars, for now) while
//! you're live, so leaked personal info can't end up on stream.
//!
//! Whether it's *active* is a process-wide flag (like `bks_core::is_dark_theme`)
//! so render code anywhere can check it without threading state. What drives the
//! flag is the persisted [`StreamerModeChoice`](crate::settings::StreamerModeChoice):
//! forced on, forced off, or Auto — active while a known broadcast app (OBS etc.)
//! is running, detected by polling the process list every [`POLL_INTERVAL`]
//! (there is no OS "OBS launched" event to subscribe to).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How often the app re-checks the process list for broadcast software. The
/// scan is a single Toolhelp snapshot (~1ms), so a short interval is cheap and
/// makes Auto mode follow OBS opening/closing promptly.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether streamer mode is currently active (render code checks this).
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Sets the process-wide active flag (only `BackseaterApp::apply_streamer_mode`
/// should call this — it also re-renders the views that read the flag).
pub fn set_active(on: bool) {
    ACTIVE.store(on, Ordering::Relaxed);
}

/// Broadcast-app executable names (lowercase), same set Chatterino watches for.
const BROADCAST_BINARIES: &[&str] = &[
    "obs.exe",
    "obs64.exe",
    "obs32.exe",
    "prismlivestudio.exe",
    "xsplit.core.exe",
    "twitchstudio.exe",
    "vmix64.exe",
    "streamlabs obs.exe",
];

/// Whether a known broadcast app is running right now, via a Toolhelp process
/// snapshot (cheap: one syscall + a name scan, no per-process handle opens).
#[cfg(windows)]
pub fn broadcast_software_running() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
                if BROADCAST_BINARIES.contains(&name.as_str()) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

#[cfg(not(windows))]
pub fn broadcast_software_running() -> bool {
    false
}
