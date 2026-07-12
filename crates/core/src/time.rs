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

/// Parses a human-typed duration into seconds: a bare number is seconds
/// (`"600"`), a number can carry a unit (`"90s"`, `"10m"`, `"2h"`, `"3d"`,
/// `"1w"`), and unit terms chain (`"1h30m"`, `"1d12h"`). Whitespace between
/// terms is tolerated (`"1h 30m"`). Returns `None` for anything else, including
/// an empty string, a zero total, and overflow. Used by the usercard's custom
/// timeout box and `/timeout`.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let mut total: u64 = 0;
    let mut chars = s.char_indices().peekable();
    let mut any = false;
    while let Some(&(start, ch)) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if !ch.is_ascii_digit() {
            return None;
        }
        let mut end = start;
        while let Some(&(i, c)) = chars.peek() {
            if c.is_ascii_digit() {
                end = i + c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let n: u64 = s[start..end].parse().ok()?;
        let unit = match chars.peek().map(|&(_, c)| c) {
            Some('s') | Some('S') => 1,
            Some('m') | Some('M') => 60,
            Some('h') | Some('H') => 3600,
            Some('d') | Some('D') => 86400,
            Some('w') | Some('W') => 604800,
            // No unit: seconds, but only as the whole term's tail ("1h30" is
            // more likely a typo'd "1h30m" than 1h + 30s — reject it).
            None => {
                if any {
                    return None;
                }
                1
            }
            Some(_) => return None,
        };
        if chars.peek().is_some() {
            chars.next();
        }
        total = total.checked_add(n.checked_mul(unit)?)?;
        any = true;
    }
    if !any || total == 0 {
        return None;
    }
    Some(total)
}

/// Formats a second count compactly, the inverse of [`parse_duration`]:
/// `90` → `"1m30s"`, `600` → `"10m"`, `5400` → `"1h30m"`, `0` → `"0s"`. Used
/// by the chat-mode bar ("Slow (5s)", "Followers-only (10m)") and anywhere a
/// duration is shown back to the user.
pub fn format_duration(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_string();
    }
    let mut out = String::new();
    let mut rest = secs;
    for (unit, label) in [(604_800, 'w'), (86_400, 'd'), (3600, 'h'), (60, 'm'), (1, 's')] {
        let n = rest / unit;
        if n > 0 {
            out.push_str(&format!("{n}{label}"));
            rest %= unit;
        }
    }
    out
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
    fn parse_duration_bare_seconds_and_units() {
        assert_eq!(parse_duration("600"), Some(600));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("3d"), Some(259_200));
        assert_eq!(parse_duration("1w"), Some(604_800));
        assert_eq!(parse_duration("2W"), Some(1_209_600));
    }

    #[test]
    fn parse_duration_compound_terms() {
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("1d12h"), Some(129_600));
        assert_eq!(parse_duration(" 1h 30m "), Some(5400));
        assert_eq!(parse_duration("1m30s"), Some(90));
    }

    #[test]
    fn parse_duration_rejects_junk() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("  "), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("10x"), None);
        assert_eq!(parse_duration("-5m"), None);
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("0m"), None);
        // A unitless tail after a united term is ambiguous — rejected.
        assert_eq!(parse_duration("1h30"), None);
        // Overflow-safe.
        assert_eq!(parse_duration("99999999999999999999w"), None);
    }

    #[test]
    fn format_duration_compact_and_roundtrips() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(90), "1m30s");
        assert_eq!(format_duration(600), "10m");
        assert_eq!(format_duration(5400), "1h30m");
        assert_eq!(format_duration(1_209_600), "2w");
        assert_eq!(parse_duration(&format_duration(129_600)), Some(129_600));
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
