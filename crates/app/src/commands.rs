//! The slash-command registry: every `/command` the input box understands, with
//! the platform(s) it works on — the single source the autocomplete popup lists
//! from and the parsers (controller + chatview intercepts) validate against.
//!
//! A future custom-command feature (user-defined macros like `/ws` → a canned
//! `/warn ... spam`) layers on top of this: it adds user entries next to
//! [`COMMANDS`] rather than replacing it, so keep lookups going through
//! [`matching`] instead of scanning the array directly.

use bks_core::Platform;

/// One slash command: its canonical name (no `/`), alternate names the parser
/// also accepts, a usage line for the popup, and the platforms it exists on.
pub struct CommandDef {
    pub name: &'static str,
    /// Alternate spellings (`/untimeout` for unban, `/user` for usercard).
    /// Accepted when typed, and prefix-matched in the popup (shown under the
    /// canonical entry's usage/description).
    pub aliases: &'static [&'static str],
    /// Shown as the popup row's main line, e.g. "/timeout <user> <duration>".
    pub usage: &'static str,
    /// One-line explanation, muted under the usage.
    pub description: &'static str,
    pub platforms: &'static [Platform],
    /// Whether the command needs moderator powers — the popup hides these
    /// unless the user can moderate the target platform's channel (typing one
    /// anyway still runs it; the API refuses with the real error).
    pub mod_only: bool,
    /// Whether the command needs to be the *broadcaster* (Twitch only lets the
    /// channel owner raid or grant roles) — hidden from mere mods the same way.
    pub broadcaster_only: bool,
}

const TWITCH: &[Platform] = &[Platform::Twitch];
const TWITCH_KICK: &[Platform] = &[Platform::Twitch, Platform::Kick];

/// Every built-in command, alphabetical by canonical name (the popup shows them
/// in this order for an empty stem).
pub const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "announce",
        aliases: &[],
        usage: "/announce <message>",
        description: "Post a highlighted announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announceblue",
        aliases: &[],
        usage: "/announceblue <message>",
        description: "Post a blue announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announcegreen",
        aliases: &[],
        usage: "/announcegreen <message>",
        description: "Post a green announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announceorange",
        aliases: &[],
        usage: "/announceorange <message>",
        description: "Post an orange announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announcepurple",
        aliases: &[],
        usage: "/announcepurple <message>",
        description: "Post a purple announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "ban",
        aliases: &[],
        usage: "/ban <user> [reason]",
        description: "Ban a user from chat",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "chatters",
        aliases: &["viewers"],
        usage: "/chatters",
        description: "Open the viewer list (mods only)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "clear",
        aliases: &[],
        usage: "/clear",
        description: "Clear the chat history",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "delete",
        aliases: &[],
        usage: "/delete <message-id>",
        description: "Delete a single message",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "emoteonly",
        aliases: &[],
        usage: "/emoteonly",
        description: "Restrict chat to emote-only messages",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "emoteonlyoff",
        aliases: &[],
        usage: "/emoteonlyoff",
        description: "Turn off emote-only mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "followers",
        aliases: &[],
        usage: "/followers [duration]",
        description: "Followers-only chat (optional min follow age)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "followersoff",
        aliases: &[],
        usage: "/followersoff",
        description: "Turn off followers-only mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "me",
        aliases: &[],
        usage: "/me <message>",
        description: "Send an action message",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: false,
    },
    CommandDef {
        name: "mod",
        aliases: &[],
        usage: "/mod <user>",
        description: "Grant moderator (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "pin",
        aliases: &[],
        usage: "/pin [duration] <message>",
        description: "Send a message and pin it (default: until stream ends)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "raid",
        aliases: &[],
        usage: "/raid <channel>",
        description: "Start a raid (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "shoutout",
        aliases: &[],
        usage: "/shoutout <channel>",
        description: "Send an official shoutout",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "slow",
        aliases: &[],
        usage: "/slow [seconds]",
        description: "Slow mode (default 30s between messages)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "slowoff",
        aliases: &[],
        usage: "/slowoff",
        description: "Turn off slow mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "subscribers",
        aliases: &[],
        usage: "/subscribers",
        description: "Restrict chat to subscribers",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "subscribersoff",
        aliases: &[],
        usage: "/subscribersoff",
        description: "Turn off subscribers-only mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "timeout",
        aliases: &[],
        usage: "/timeout <user> <duration>",
        description: "Time a user out (600, 30m, 1h, 3d, 1w)",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "unban",
        aliases: &["untimeout"],
        usage: "/unban <user>",
        description: "Lift a ban or timeout",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "uniquechat",
        aliases: &[],
        usage: "/uniquechat",
        description: "Require unique messages",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "uniquechatoff",
        aliases: &[],
        usage: "/uniquechatoff",
        description: "Turn off unique-chat mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "unmod",
        aliases: &[],
        usage: "/unmod <user>",
        description: "Revoke moderator (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "unpin",
        aliases: &[],
        usage: "/unpin",
        description: "Remove the current pinned message",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "unraid",
        aliases: &[],
        usage: "/unraid",
        description: "Cancel a pending raid",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "unvip",
        aliases: &[],
        usage: "/unvip <user>",
        description: "Revoke VIP (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "usercard",
        aliases: &["user"],
        usage: "/usercard <user>",
        description: "Open a chatter's usercard",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: false,
    },
    CommandDef {
        name: "vip",
        aliases: &[],
        usage: "/vip <user>",
        description: "Grant VIP (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "warn",
        aliases: &[],
        usage: "/warn <user> <reason>",
        description: "Warn a user (they must acknowledge it)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
];

/// The commands available on `platform` whose canonical name (or an alias)
/// starts with `stem` (already-typed text after the `/`, matched
/// case-insensitively) — the autocomplete popup's candidate list.
pub fn matching(platform: Platform, stem: &str) -> Vec<&'static CommandDef> {
    let stem = stem.to_lowercase();
    COMMANDS
        .iter()
        .filter(|c| c.platforms.contains(&platform))
        .filter(|c| {
            c.name.starts_with(&stem) || c.aliases.iter().any(|a| a.starts_with(&stem))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_filters_by_platform() {
        // /warn is Twitch-only; /ban exists on both.
        assert!(matching(Platform::Twitch, "warn").iter().any(|c| c.name == "warn"));
        assert!(matching(Platform::Kick, "warn").is_empty());
        assert!(matching(Platform::Kick, "ban").iter().any(|c| c.name == "ban"));
    }

    #[test]
    fn matching_prefix_matches_names_and_aliases() {
        let slow: Vec<&str> = matching(Platform::Twitch, "slow")
            .iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(slow, ["slow", "slowoff"]);
        // "viewers" is an alias of chatters — the prefix must still surface it.
        assert!(matching(Platform::Twitch, "view").iter().any(|c| c.name == "chatters"));
        // Case-insensitive.
        assert!(matching(Platform::Twitch, "SLOW").iter().any(|c| c.name == "slow"));
        // Empty stem lists everything for the platform.
        assert_eq!(
            matching(Platform::Twitch, "").len(),
            COMMANDS
                .iter()
                .filter(|c| c.platforms.contains(&Platform::Twitch))
                .count()
        );
    }

    #[test]
    fn broadcaster_only_marks_exactly_the_owner_commands() {
        let owner: Vec<&str> = COMMANDS
            .iter()
            .filter(|c| c.broadcaster_only)
            .map(|c| c.name)
            .collect();
        assert_eq!(owner, ["mod", "raid", "unmod", "unraid", "unvip", "vip"]);
        // Owner-only implies mod-only (the popup filters compose).
        assert!(COMMANDS
            .iter()
            .filter(|c| c.broadcaster_only)
            .all(|c| c.mod_only));
    }

    #[test]
    fn matching_covers_aliases_exactly() {
        // A fully typed alias still matches its canonical entry.
        assert!(matching(Platform::Twitch, "untimeout")
            .iter()
            .any(|c| c.name == "unban"));
        assert!(matching(Platform::Kick, "user").iter().any(|c| c.name == "usercard"));
        assert!(matching(Platform::Twitch, "nosuch").is_empty());
    }
}
