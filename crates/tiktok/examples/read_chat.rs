//! Read-only smoke test of the same connector used by the desktop app.
use bks_platform::{ChatEvent, ChatSource};
use bks_tiktok::TikTokSource;
use std::{process::ExitCode, time::Duration};

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let channel = std::env::args().nth(1).ok_or_else(|| {
        anyhow::anyhow!(
            "Usage: cargo run -p bks-tiktok --example read_chat -- <username> [seconds]"
        )
    })?;
    let seconds = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 300);
    let mut stream = TikTokSource.join(&channel).await?;
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    let mut messages = 0u64;
    let mut history = 0u64;
    let mut errors = 0u64;
    loop {
        let event = tokio::select! {
            _ = &mut deadline => break,
            _ = tokio::signal::ctrl_c() => break,
            event = stream.recv() => event,
        };
        match event {
            Some(ChatEvent::Message(msg)) => {
                if msg.historical {
                    history += 1;
                } else {
                    messages += 1;
                }
                if messages + history <= 3 {
                    println!(
                        "{}: {} (history={})",
                        msg.author.display_name, msg.raw_text, msg.historical
                    );
                }
            }
            Some(ChatEvent::Live { live, title, .. }) => println!("live={live} title={title}"),
            Some(ChatEvent::Viewers { count, .. }) => println!("viewers={count:?}"),
            Some(ChatEvent::Error(error)) => {
                errors += 1;
                eprintln!("{error}");
            }
            None => break,
            _ => {}
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(100)).await;
    println!("Summary: {messages} live messages, {history} history messages, {errors} errors");
    Ok(if messages > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
