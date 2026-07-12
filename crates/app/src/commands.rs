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
    /// Accepted when typed, and listed in the popup as their own rows (usage
    /// rewritten to the alias, the canonical entry's description).
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
        // Twitch = Helix; Kick = public `DELETE /chat/{id}` (needs the
        // moderation:chat_message:manage scope — re-login on older tokens).
        description: "Delete a single message",
        platforms: TWITCH_KICK,
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
        usage: "/timeout <user> <duration> [reason]",
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
        // Twitch-only, like /delete — Kick has no public unpin endpoint.
        description: "Remove the current pinned message",
        platforms: TWITCH,
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

/// What a command implicitly acts on — derived from its usage line's first
/// argument (`<user>` / `<message-id>`), so the registry stays the single
/// source of truth. The mod buttons use this to spare users the placeholders:
/// a button command like "/timeout 600 spam" (no `{user}`/`{msg-id}` typed)
/// gets the row's target inserted right after the command name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImplicitTarget {
    User,
    MessageId,
}

/// The implicit target of the command named `name` (an alias works too), or
/// `None` for commands that don't lead with `<user>`/`<message-id>` — those
/// need explicit placeholders on a mod button (or take no target at all).
pub fn implicit_target(name: &str) -> Option<ImplicitTarget> {
    let name = name.to_lowercase();
    let cmd = COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name.as_str()))?;
    match cmd.usage.split_whitespace().nth(1) {
        Some("<user>") => Some(ImplicitTarget::User),
        Some("<message-id>") => Some(ImplicitTarget::MessageId),
        _ => None,
    }
}

/// Whether a mod button's command template needs the row's real message id:
/// an explicit `{msg-id}`, or a leading known command whose implicit target is
/// the message (a bare "/delete"). Such buttons ghost on local-echo rows —
/// their synthetic id is accepted by no API.
pub fn needs_msg_id(template: &str) -> bool {
    if template.contains("{msg-id}") {
        return true;
    }
    if template.contains("{user}") {
        return false;
    }
    template
        .strip_prefix('/')
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(implicit_target)
        == Some(ImplicitTarget::MessageId)
}

/// Whether a mod button's command template can run on `platform`: a leading
/// known slash command must list the platform in the registry (e.g. a bare
/// "/delete" can't run on Kick rows, so the button ghosts there); plain text
/// (sent to chat) and unknown commands aren't platform-gated.
pub fn supported_on(template: &str, platform: Platform) -> bool {
    let Some(rest) = template.trim_start().strip_prefix('/') else {
        return true;
    };
    let name = rest.split_whitespace().next().unwrap_or("").to_lowercase();
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name.as_str()))
        .is_none_or(|c| c.platforms.contains(&platform))
}

/// One autocomplete candidate: a command under one of its spellings. Aliases
/// get their own row (their own name + usage line) so `/untimeout` is
/// discoverable in the popup instead of folded invisibly into `/unban`.
#[derive(Clone, Copy)]
pub struct CommandMatch {
    /// The matched spelling (canonical name or an alias) — what the popup
    /// shows and inserts.
    pub name: &'static str,
    pub def: &'static CommandDef,
}

impl CommandMatch {
    /// The usage line under this spelling: the def's usage with its leading
    /// `/name` swapped for the matched one (`/untimeout <user>`).
    pub fn usage(&self) -> String {
        match self.def.usage.strip_prefix(&format!("/{}", self.def.name)) {
            Some(rest) => format!("/{}{rest}", self.name),
            None => self.def.usage.to_string(),
        }
    }
}

/// The command spellings available on `platform` that start with `stem`
/// (already-typed text after the `/`, matched case-insensitively) — the
/// autocomplete popup's candidate list, alphabetical, canonical names and
/// aliases each as their own entry.
pub fn matching(platform: Platform, stem: &str) -> Vec<CommandMatch> {
    let stem = stem.to_lowercase();
    let mut matches: Vec<CommandMatch> = COMMANDS
        .iter()
        .filter(|c| c.platforms.contains(&platform))
        .flat_map(|def| {
            std::iter::once(def.name)
                .chain(def.aliases.iter().copied())
                .map(move |name| CommandMatch { name, def })
        })
        .filter(|m| m.name.starts_with(&stem))
        .collect();
    matches.sort_by_key(|m| m.name);
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_target_follows_the_usage_shape() {
        assert_eq!(implicit_target("timeout"), Some(ImplicitTarget::User));
        assert_eq!(implicit_target("ban"), Some(ImplicitTarget::User));
        assert_eq!(implicit_target("warn"), Some(ImplicitTarget::User));
        // Aliases and case resolve like canonical names.
        assert_eq!(implicit_target("untimeout"), Some(ImplicitTarget::User));
        assert_eq!(implicit_target("BAN"), Some(ImplicitTarget::User));
        assert_eq!(implicit_target("delete"), Some(ImplicitTarget::MessageId));
        // No leading <user>/<message-id> argument → no implicit target.
        assert_eq!(implicit_target("announce"), None);
        assert_eq!(implicit_target("slow"), None);
        assert_eq!(implicit_target("raid"), None); // <channel>, not <user>
        assert_eq!(implicit_target("nonsense"), None);
    }

    #[test]
    fn needs_msg_id_covers_explicit_and_implicit_forms() {
        assert!(needs_msg_id("/delete"));
        assert!(needs_msg_id("/delete {msg-id}"));
        assert!(needs_msg_id("!log {msg-id}"));
        assert!(!needs_msg_id("/ban"));
        assert!(!needs_msg_id("/timeout 600 spam"));
        assert!(!needs_msg_id("!so {user}"));
        assert!(!needs_msg_id("plain chat text"));
    }

    #[test]
    fn supported_on_gates_known_commands_by_platform() {
        // /delete works on both (Kick added the public endpoint); /unpin and
        // /warn are Twitch-only.
        assert!(supported_on("/delete", Platform::Kick));
        assert!(!supported_on("/unpin", Platform::Kick));
        assert!(!supported_on("/warn {user} spam", Platform::Kick));
        assert!(supported_on("/warn {user} spam", Platform::Twitch));
        // Plain text and unknown commands aren't platform-gated.
        assert!(supported_on("!so {user}", Platform::Kick));
        assert!(supported_on("/notacommand", Platform::Kick));
    }

    #[test]
    fn matching_filters_by_platform() {
        // /warn is Twitch-only; /ban exists on both.
        assert!(matching(Platform::Twitch, "warn").iter().any(|m| m.name == "warn"));
        assert!(matching(Platform::Kick, "warn").is_empty());
        assert!(matching(Platform::Kick, "ban").iter().any(|m| m.name == "ban"));
    }

    #[test]
    fn matching_prefix_matches_names_and_aliases() {
        let slow: Vec<&str> = matching(Platform::Twitch, "slow")
            .iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(slow, ["slow", "slowoff"]);
        // "viewers" is an alias of chatters — the prefix surfaces it as its
        // OWN row (typed spelling shown/inserted, usage rewritten to it).
        let viewers = matching(Platform::Twitch, "view");
        assert!(viewers.iter().any(|m| m.name == "viewers" && m.def.name == "chatters"));
        assert_eq!(
            matching(Platform::Twitch, "unt")
                .iter()
                .map(|m| m.usage())
                .collect::<Vec<_>>(),
            ["/untimeout <user>"]
        );
        // Case-insensitive.
        assert!(matching(Platform::Twitch, "SLOW").iter().any(|m| m.name == "slow"));
        // Empty stem lists every spelling for the platform, alphabetically.
        let all = matching(Platform::Twitch, "");
        assert_eq!(
            all.len(),
            COMMANDS
                .iter()
                .filter(|c| c.platforms.contains(&Platform::Twitch))
                .map(|c| 1 + c.aliases.len())
                .sum::<usize>()
        );
        assert!(all.windows(2).all(|w| w[0].name < w[1].name));
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
        // A fully typed alias still matches, carrying its canonical def.
        assert!(matching(Platform::Twitch, "untimeout")
            .iter()
            .any(|m| m.def.name == "unban"));
        assert!(matching(Platform::Kick, "user").iter().any(|m| m.def.name == "usercard"));
        assert!(matching(Platform::Twitch, "nosuch").is_empty());
    }
}
