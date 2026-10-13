//! Read-only smoke test of the same Kick connector used by the desktop app.
use bks_kick::KickSource;
use bks_platform::{ChatEvent, ChatSource};
use std::{collections::HashSet, process::ExitCode, time::Duration};

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let channel = std::env::args().nth(1).ok_or_else(|| {
        anyhow::anyhow!(
            "Usage: cargo run -p bks-kick --example read_kick_chat -- <channel> [seconds]"
        )
    })?;
    let seconds = std::env::args()
        .nth(2)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 300);
    let mut stream = KickSource::new().join(&channel).await?;
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut live = HashSet::new();
    let mut history = 0;
    let mut errors = 0;
    let mut connected = false;
    loop {
        let event = tokio::select! {
            _ = &mut deadline => break,
            event = stream.recv() => event,
        };
        match event {
            Some(ChatEvent::System(text)) => {
                connected = true;
                println!("{text}");
            }
            Some(ChatEvent::Message(message)) if message.historical => history += 1,
            Some(ChatEvent::Message(message)) => {
                if live.insert(message.id.clone()) && live.len() <= 3 {
                    // Timestamp age includes local/server clock skew; this is
                    // diagnostic context, not a cross-transport latency measure.
                    let age = (chrono::Utc::now() - message.timestamp).num_milliseconds();
                    println!("live id={} timestamp_age_ms={age}", message.id);
                }
            }
            Some(ChatEvent::Error(error)) => {
                errors += 1;
                eprintln!("{error}");
            }
            None => break,
            _ => {}
        }
    }
    println!(
        "Summary: connected={connected}, {} unique live messages, {history} history messages, {errors} errors",
        live.len()
    );
    Ok(if connected && errors == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
