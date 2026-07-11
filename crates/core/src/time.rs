//! Small shared time helpers. Several connectors parse RFC-3339 timestamps from
//! their APIs (Twitch IVR, Kick chat/channel, the usercard); this is the one place
//! that turns such a string into a UTC instant.

use chrono::{DateTime, Utc};

/// The delay before reconnect attempt number `attempt` (0-based): exponential
/// from 2s, capped at 60s. Shared by every connector's reconnect loop so they
/// back off identically.
pub fn reconnect_delay(attempt: u32) -> std::time::Duration {
    let secs = 2u64.saturating_mul(1u64 << attempt.min(5)); // 2,4,8,16,32,64…
    std::time::Duration::from_secs(secs.min(60))
}

/// Parses an RFC-3339 timestamp into a UTC instant, returning `None` on an empty
/// or unparseable string.
pub fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn parses_valid_and_rejects_bad() {
        let dt = parse_rfc3339("2026-06-28T12:00:56Z").expect("valid");
        assert_eq!(dt.hour(), 12);
        assert!(parse_rfc3339("").is_none());
        assert!(parse_rfc3339("not a date").is_none());
    }

    #[test]
    fn reconnect_delay_backs_off_and_caps() {
        assert_eq!(reconnect_delay(0).as_secs(), 2);
        assert_eq!(reconnect_delay(1).as_secs(), 4);
        assert_eq!(reconnect_delay(4).as_secs(), 32);
        assert_eq!(reconnect_delay(5).as_secs(), 60);
        assert_eq!(reconnect_delay(u32::MAX).as_secs(), 60);
    }
}
