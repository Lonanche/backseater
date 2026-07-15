//! Small text-formatting helpers shared by connectors when building notices.

/// Picks the singular or plural form for a count: `plural(1, "month", "months")`
/// → `"month"`, `plural(2, …)` → `"months"`. Connectors use it for sub/gift/
/// viewer/Kick counts so the "is it 1?" check isn't hand-written everywhere.
pub fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// Formats a count with thousands separators (`1234567` → `"1,234,567"`) —
/// viewer readouts, raid sizes, bits-badge tiers.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Formats a count abbreviated with a K/M/B suffix (`1234` → `"1.2K"`,
/// `1_500_000` → `"1.5M"`, `2_000_000_000` → `"2B"`) — for compact stat lines
/// like a video's "N views". Whole thousands drop the decimal (`12_000` →
/// `"12K"`, not `"12.0K"`). Distinct from [`format_count`], which groups with
/// commas (`"1,234,567"`).
pub fn format_count_compact(n: u64) -> String {
    const UNITS: &[(u64, char)] = &[(1_000_000_000, 'B'), (1_000_000, 'M'), (1_000, 'K')];
    for &(threshold, suffix) in UNITS {
        if n >= threshold {
            let value = n as f64 / threshold as f64;
            let rounded = (value * 10.0).round() / 10.0;
            if rounded.fract().abs() < f64::EPSILON {
                return format!("{}{}", rounded as u64, suffix);
            }
            return format!("{rounded:.1}{suffix}");
        }
    }
    n.to_string()
}

/// Strips a channel name to its bare form: trims surrounding whitespace and a
/// leading `#` (as Twitch IRC uses), preserving case. Use this when you need the
/// channel's display name; use [`channel_login`] for API/lookup keys.
pub fn strip_channel(channel: &str) -> &str {
    channel.trim().trim_start_matches('#')
}

/// The lookup form of a channel name: [`strip_channel`] then lowercased, for IRC
/// joins, Helix/Kick API calls, and id maps where case must not matter.
pub fn channel_login(channel: &str) -> String {
    strip_channel(channel).to_lowercase()
}

/// Percent-encodes `s` for use as one URL path segment or query value: RFC 3986
/// unreserved characters (alphanumerics, `-_.~`) pass through, everything else is
/// `%XX`-encoded per UTF-8 byte. Logins/slugs interpolated into request URLs come
/// from user-typed commands and tab config, so they can contain anything —
/// unencoded, a stray `/`, `?`, or `&` would rewrite the request.
pub fn encode_url_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_picks_form() {
        assert_eq!(plural(1, "month", "months"), "month");
        assert_eq!(plural(0, "month", "months"), "months");
        assert_eq!(plural(2, "month", "months"), "months");
    }

    #[test]
    fn strip_channel_trims_hash_keeps_case() {
        assert_eq!(strip_channel("  #ChanName "), "ChanName");
        assert_eq!(strip_channel("plain"), "plain");
    }

    #[test]
    fn channel_login_lowercases() {
        assert_eq!(channel_login("  #ChanName "), "channame");
    }

    #[test]
    fn format_count_groups_thousands() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(12345), "12,345");
        assert_eq!(format_count(1234567), "1,234,567");
    }

    #[test]
    fn format_count_compact_abbreviates() {
        assert_eq!(format_count_compact(5), "5");
        assert_eq!(format_count_compact(999), "999");
        assert_eq!(format_count_compact(1_200), "1.2K");
        assert_eq!(format_count_compact(12_000), "12K"); // whole → no ".0"
        assert_eq!(format_count_compact(1_500_000), "1.5M");
        assert_eq!(format_count_compact(2_000_000_000), "2B");
    }

    #[test]
    fn encode_url_component_passes_unreserved_and_encodes_the_rest() {
        assert_eq!(
            encode_url_component("normal_login-1.x~"),
            "normal_login-1.x~"
        );
        assert_eq!(encode_url_component("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
        assert_eq!(encode_url_component("spa ce"), "spa%20ce");
        // Multi-byte UTF-8 is encoded per byte.
        assert_eq!(encode_url_component("é"), "%C3%A9");
    }
}
