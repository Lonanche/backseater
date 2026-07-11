//! The mention alert sound: a short bundled ping (synthesized, ours — see
//! `assets/sounds/ping.wav`), played fire-and-forget via the Win32 `PlaySound`
//! API straight from the embedded bytes. Chatterino-style: the sound ships in
//! the binary, not a system sound. No-op on other platforms until a
//! cross-platform audio backend is needed.

#[cfg(windows)]
pub(crate) fn play_mention_ping() {
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
        tracing::debug!("mention ping failed to play");
    }
}

#[cfg(not(windows))]
pub(crate) fn play_mention_ping() {}
