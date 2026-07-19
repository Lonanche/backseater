//! The alert sound (mention + channel-event pings): a short bundled ping
//! (synthesized, ours — see `assets/sounds/ping.wav`), played fire-and-forget
//! via the Win32 `PlaySound` API straight from the embedded bytes.
//! Chatterino-style: the sound ships in the binary, not a system sound. No-op
//! on other platforms until a cross-platform audio backend is needed.

/// Plays the alert ping unless streamer mode is muting sounds — the one gate
/// every ping shares, so no call site can forget it. Callers layer their own
/// enablement (mention master toggle / per-term mute / per-kind event bells)
/// on top.
pub(crate) fn play_ping() {
    if crate::streamer_mode::is_active() && crate::settings::streamer_mute_sounds() {
        return;
    }
    play_ping_raw();
}

#[cfg(windows)]
fn play_ping_raw() {
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

    static PING: &[u8] = include_bytes!("../assets/sounds/ping.wav");
    // SND_MEMORY reinterprets the "name" pointer as the in-memory wav; ASYNC
    // returns immediately (a new ping cuts off a still-playing one, fine for
    // an alert); NODEFAULT keeps a decode failure silent instead of beeping.
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(PING.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        tracing::debug!("alert ping failed to play");
    }
}

#[cfg(not(windows))]
fn play_ping_raw() {}
