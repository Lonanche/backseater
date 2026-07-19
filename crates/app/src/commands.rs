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
    /// The scopes the logged-in *Twitch* token needs to run this (empty = none:
    /// IRC/UI-only commands). Logins request a user-chosen scope tier
    /// (Settings → Account), so the popup hides commands the token can't run —
    /// like `mod_only` for non-mods — via `session::twitch_scope_missing`.
    /// Kick's scope set is fixed at login, so it isn't gated here.
    pub twitch_scopes: &'static [&'static str],
}

const TWITCH: &[Platform] = &[Platform::Twitch];
const TWITCH_KICK: &[Platform] = &[Platform::Twitch, Platform::Kick];

// Named scope slices — every entry below uses one, and the `pub` ones are the
// single source the ad-hoc UI gates reference too (`ChatView::can_pin`, the
// viewer-list button, the usercard panels), so a scope rename can't silently
// diverge between the registry and a hand-copied literal.
const NO_SCOPES: &[&str] = &[];
pub const SCOPE_BANNED_USERS: &[&str] = &["moderator:manage:banned_users"];
pub const SCOPE_CHAT_MESSAGES: &[&str] = &["moderator:manage:chat_messages"];
pub const SCOPE_CHATTERS: &[&str] = &["moderator:read:chatters"];
pub const SCOPE_MODERATORS: &[&str] = &["channel:manage:moderators"];
pub const SCOPE_VIPS: &[&str] = &["channel:manage:vips"];
pub const SCOPE_WARNINGS: &[&str] = &["moderator:manage:warnings"];
const SCOPE_CHAT_SETTINGS: &[&str] = &["moderator:manage:chat_settings"];
const SCOPE_ANNOUNCEMENTS: &[&str] = &["moderator:manage:announcements"];
const SCOPE_SUSPICIOUS: &[&str] = &["moderator:manage:suspicious_users"];
const SCOPE_RAIDS: &[&str] = &["channel:manage:raids"];
const SCOPE_SHOUTOUTS: &[&str] = &["moderator:manage:shoutouts"];

/// Every built-in command, alphabetical by canonical name (the popup shows them
/// in this order for an empty stem).
pub const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "announce",
        twitch_scopes: SCOPE_ANNOUNCEMENTS,
        aliases: &[],
        usage: "/announce <message>",
        description: "Post a highlighted announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announceblue",
        twitch_scopes: SCOPE_ANNOUNCEMENTS,
        aliases: &[],
        usage: "/announceblue <message>",
        description: "Post a blue announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announcegreen",
        twitch_scopes: SCOPE_ANNOUNCEMENTS,
        aliases: &[],
        usage: "/announcegreen <message>",
        description: "Post a green announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announceorange",
        twitch_scopes: SCOPE_ANNOUNCEMENTS,
        aliases: &[],
        usage: "/announceorange <message>",
        description: "Post an orange announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "announcepurple",
        twitch_scopes: SCOPE_ANNOUNCEMENTS,
        aliases: &[],
        usage: "/announcepurple <message>",
        description: "Post a purple announcement",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "ban",
        twitch_scopes: SCOPE_BANNED_USERS,
        aliases: &[],
        usage: "/ban <user> [reason]",
        description: "Ban a user from chat",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "chatters",
        twitch_scopes: SCOPE_CHATTERS,
        aliases: &["viewers"],
        usage: "/chatters",
        description: "Open the viewer list (mods only)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "clear",
        twitch_scopes: SCOPE_CHAT_MESSAGES,
        aliases: &[],
        usage: "/clear",
        description: "Clear the chat history",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "delete",
        twitch_scopes: SCOPE_CHAT_MESSAGES,
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
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/emoteonly",
        description: "Restrict chat to emote-only messages",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "emoteonlyoff",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/emoteonlyoff",
        description: "Turn off emote-only mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "followers",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/followers [duration]",
        description: "Followers-only chat (optional min follow age)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "followersoff",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/followersoff",
        description: "Turn off followers-only mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "me",
        twitch_scopes: NO_SCOPES,
        aliases: &[],
        usage: "/me <message>",
        description: "Send an action message",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: false,
    },
    CommandDef {
        name: "mod",
        twitch_scopes: SCOPE_MODERATORS,
        aliases: &[],
        usage: "/mod <user>",
        description: "Grant moderator (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "pin",
        twitch_scopes: &["user:write:chat", "moderator:manage:chat_messages"],
        aliases: &[],
        usage: "/pin [duration] <message>",
        description: "Send a message and pin it (default: until stream ends)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "raid",
        twitch_scopes: SCOPE_RAIDS,
        aliases: &[],
        usage: "/raid <channel>",
        description: "Start a raid (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "shoutout",
        twitch_scopes: SCOPE_SHOUTOUTS,
        aliases: &[],
        usage: "/shoutout <channel>",
        description: "Send an official shoutout",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "slow",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/slow [seconds]",
        description: "Slow mode (default 30s between messages)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "slowoff",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/slowoff",
        description: "Turn off slow mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "subscribers",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/subscribers",
        description: "Restrict chat to subscribers",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "subscribersoff",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/subscribersoff",
        description: "Turn off subscribers-only mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "timeout",
        twitch_scopes: SCOPE_BANNED_USERS,
        aliases: &[],
        usage: "/timeout <user> <duration> [reason]",
        description: "Time a user out (600, 30m, 1h, 3d, 1w)",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "unban",
        twitch_scopes: SCOPE_BANNED_USERS,
        aliases: &["untimeout"],
        usage: "/unban <user>",
        description: "Lift a ban or timeout",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "uniquechat",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/uniquechat",
        description: "Require unique messages",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "uniquechatoff",
        twitch_scopes: SCOPE_CHAT_SETTINGS,
        aliases: &[],
        usage: "/uniquechatoff",
        description: "Turn off unique-chat mode",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "unmod",
        twitch_scopes: SCOPE_MODERATORS,
        aliases: &[],
        usage: "/unmod <user>",
        description: "Revoke moderator (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "unpin",
        twitch_scopes: SCOPE_CHAT_MESSAGES,
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
        twitch_scopes: SCOPE_RAIDS,
        aliases: &[],
        usage: "/unraid",
        description: "Cancel a pending raid",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "unvip",
        twitch_scopes: SCOPE_VIPS,
        aliases: &[],
        usage: "/unvip <user>",
        description: "Revoke VIP (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "usercard",
        twitch_scopes: NO_SCOPES,
        aliases: &["user"],
        usage: "/usercard <user>",
        description: "Open a chatter's usercard",
        platforms: TWITCH_KICK,
        broadcaster_only: false,
        mod_only: false,
    },
    CommandDef {
        name: "vip",
        twitch_scopes: SCOPE_VIPS,
        aliases: &[],
        usage: "/vip <user>",
        description: "Grant VIP (broadcaster only)",
        platforms: TWITCH,
        broadcaster_only: true,
        mod_only: true,
    },
    CommandDef {
        name: "warn",
        twitch_scopes: SCOPE_WARNINGS,
        aliases: &[],
        usage: "/warn <user> <reason>",
        description: "Warn a user (they must acknowledge it)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "monitor",
        twitch_scopes: SCOPE_SUSPICIOUS,
        aliases: &[],
        usage: "/monitor <user>",
        description: "Mark a user as a monitored suspicious user",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "restrict",
        twitch_scopes: SCOPE_SUSPICIOUS,
        aliases: &[],
        usage: "/restrict <user>",
        description: "Restrict a suspicious user (their messages are held for mod review)",
        platforms: TWITCH,
        broadcaster_only: false,
        mod_only: true,
    },
    CommandDef {
        name: "unmonitor",
        twitch_scopes: SCOPE_SUSPICIOUS,
        aliases: &["unrestrict"],
        usage: "/unmonitor <user>",
        description: "Remove a user's suspicious-user treatment",
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

/// The registry entry for `name` — a canonical name or an alias, any case.
/// The one place the name/alias match lives; every template/name lookup
/// (`implicit_target`, `supported_on`, `twitch_scopes_for_template`) goes
/// through it so they can't gate the same command differently.
fn def_for(name: &str) -> Option<&'static CommandDef> {
    let name = name.to_lowercase();
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name.as_str()))
}

/// The leading `/command` name of a template, if it starts with one.
fn leading_command(template: &str) -> Option<&str> {
    template
        .trim_start()
        .strip_prefix('/')?
        .split_whitespace()
        .next()
}

/// The implicit target of the command named `name` (an alias works too), or
/// `None` for commands that don't lead with `<user>`/`<message-id>` — those
/// need explicit placeholders on a mod button (or take no target at all).
pub fn implicit_target(name: &str) -> Option<ImplicitTarget> {
    match def_for(name)?.usage.split_whitespace().nth(1) {
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
    leading_command(template).and_then(implicit_target) == Some(ImplicitTarget::MessageId)
}

/// Whether a mod button's command template acts on a *user* (not a message):
/// an explicit `{user}`, or a leading known command whose implicit target is
/// the user (a bare "/ban", "/timeout 600"). The usercard — which targets a
/// person, not a message — offers only these buttons (a "/delete" or a
/// `{msg-id}` template has no message to act on there).
pub fn targets_user(template: &str) -> bool {
    if template.contains("{user}") {
        return true;
    }
    if template.contains("{msg-id}") {
        return false;
    }
    leading_command(template).and_then(implicit_target) == Some(ImplicitTarget::User)
}

/// Whether a mod button's command template can run on `platform`: a leading
/// known slash command must list the platform in the registry (e.g. a bare
/// "/delete" can't run on Kick rows, so the button ghosts there); plain text
/// (sent to chat) and unknown commands aren't platform-gated.
pub fn supported_on(template: &str, platform: Platform) -> bool {
    leading_command(template)
        .and_then(def_for)
        .is_none_or(|c| c.platforms.contains(&platform))
}

/// The Twitch scopes a mod-button template's leading known command needs —
/// empty for plain text and unknown commands (those aren't scope-gated). The
/// strip/usercard buttons ghost on Twitch rows when the login tier left these
/// out (`session::twitch_scope_missing`), like `supported_on` for platforms.
pub fn twitch_scopes_for_template(template: &str) -> &'static [&'static str] {
    leading_command(template)
        .and_then(def_for)
        .map_or(NO_SCOPES, |c| c.twitch_scopes)
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
        assert_eq!(implicit_target("monitor"), Some(ImplicitTarget::User));
        assert_eq!(implicit_target("unrestrict"), Some(ImplicitTarget::User));
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
    fn targets_user_covers_explicit_and_implicit_forms() {
        assert!(targets_user("/ban"));
        assert!(targets_user("/timeout 600 spam"));
        assert!(targets_user("/warn {user} spam"));
        assert!(targets_user("!so {user}"));
        // Message-targeted and no-target templates don't touch a user.
        assert!(!targets_user("/delete"));
        assert!(!targets_user("/delete {msg-id}"));
        assert!(!targets_user("/announce hi"));
        assert!(!targets_user("plain chat text"));
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
    fn twitch_scopes_cover_every_twitch_command_that_calls_helix() {
        // Every Twitch mod command goes through a scoped Helix endpoint except
        // the UI-only ones (usercard opens a window; chatters' fetch is gated
        // by its own scope). A missing entry here would leave the command
        // visible in the popup for a login tier that can't run it.
        for cmd in COMMANDS {
            if cmd.platforms.contains(&Platform::Twitch)
                && cmd.mod_only
                && cmd.name != "usercard"
            {
                assert!(
                    !cmd.twitch_scopes.is_empty(),
                    "/{} has no twitch_scopes",
                    cmd.name
                );
            }
        }
        // Spot-checks: non-mod commands stay ungated; pin needs both the Helix
        // send (it sends the message itself) and the pin call.
        assert!(twitch_scopes_for_template("/me hi").is_empty());
        assert_eq!(
            twitch_scopes_for_template("/timeout 600 spam"),
            SCOPE_BANNED_USERS
        );
        assert_eq!(
            twitch_scopes_for_template("  /untimeout {user}"),
            SCOPE_BANNED_USERS
        );
        assert_eq!(
            twitch_scopes_for_template("/pin hello"),
            &["user:write:chat", "moderator:manage:chat_messages"]
        );
        // Plain text / unknown commands aren't scope-gated.
        assert!(twitch_scopes_for_template("!so {user}").is_empty());
        assert!(twitch_scopes_for_template("/notacommand").is_empty());
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
