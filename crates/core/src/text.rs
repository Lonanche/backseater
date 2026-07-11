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
