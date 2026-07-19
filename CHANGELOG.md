# Changelog

Each `## vX.Y.Z` section becomes the GitHub release notes for that version
(extracted by `.github/workflows/ci.yml` when it auto-publishes a release).

## v0.6.0-beta.2

### Features

- Pick which channel events play a sound: every event kind in a tab's
  events-panel settings (Subscriptions, Gifts, Raids / Hosts, Bits / Kicks,
  Channel points, …) now has a bell toggle next to its checkbox, so a raid or
  a gift bomb can ping you the way a mention does. Sounds are off by default
  and per tab; a mass gift plays a single ping for the whole batch, never one
  per recipient, and an event pings once even when the same channel is open in
  several tabs or popped-out windows.
- The event-kind list (with its bells) is available even when the events panel
  itself is turned off — events still show in the chat log, and now they can
  ping without the panel.
- Streamer mode's sound muting now covers event pings too (the setting is
  labeled "Mute alert sounds while active").

### Fixes

- Changing a tab's events-panel filters now applies to that tab's popped-out
  windows too; popouts used to keep the filters they were opened with.

## v0.6.0-beta.1

### Features

- Choose what Backseater may do with your Twitch account when you log in.
  The Account settings now offer three permission tiers — **Chat only**
  (read and send messages), **Basic moderation** (ban/timeout, delete
  messages, pin messages, viewer list), and **Full moderator** (warnings,
  announcements, chat modes, the AutoMod queue, suspicious users, and the
  rich mod-action feed) — plus a separate **Broadcaster tools** add-on
  (/raid and granting mod/VIP, for your own channel). Twitch's consent
  screen lists only what you picked, so a chat-only login shows just a few
  harmless lines instead of the full moderator list.
- Anything your login tier doesn't cover is hidden in the app — mod buttons,
  the hover 📌, the viewer list, and slash-command suggestions all follow the
  permissions you actually granted instead of failing with an API error.
- The Twitch account row shows what your login can do ("basic moderation",
  "full moderator + broadcaster", …), and a new Permissions… button lets you
  re-login at a different tier any time.
- Existing logins are untouched: a token from before this version keeps
  everything visible and working exactly as before.

### Fixes

- Jumping to a chat message (from search or a mention) no longer lands it
  hidden underneath the floating pinned-message cards at the top of the log.

## v0.5.5

### Features

- Suspicious-user marks (Twitch "Low Trust", visible to moderators): messages
  from monitored or restricted chatters get a distinct row tint and a
  MONITORED / RESTRICTED tag, including Twitch's extra context like "likely
  ban evader · banned in 2 shared channels". Restricted users' messages —
  which Twitch withholds from normal chat — now appear in your feed as marked
  rows, and a mod changing someone's treatment posts a notice.
- New mod commands `/monitor`, `/restrict`, and `/unmonitor` (alias
  `/unrestrict`) set or remove a user's suspicious-user treatment; they also
  work as custom mod-button commands.
- These need new Twitch permissions, so log out and back in once to enable
  them; until then chat keeps working and the marks just stay off.

### Changes

- The chat-search shortcut moved from Ctrl+R to Ctrl+F, and now also works
  while the cursor is in the message box.

## v0.5.0

### Features

- Reply threads on Twitch and Kick. Click a reply's "↪ replying to" line to open
  a thread panel showing the whole conversation, and when you start a reply the
  bar above the message box shows the full thread you're joining. Usernames in
  both are clickable to open a usercard.
- Link previews. Twitch and Kick clips and YouTube videos posted in chat show a
  preview card — title, channel, and view count with a thumbnail — as a hover
  tooltip or an inline card under the message (Appearance → Link previews;
  Streamer Mode can hide the thumbnail while you're live).
- YouTube Super Chats, new memberships, and gifted memberships now render like
  Twitch resubs: the donor's comment appears as its own chat line under a compact
  "Name · sent $5.00" header instead of being flattened into the event text.
- A new title bar shows which accounts you're logged into on Twitch and Kick at a
  glance (dimmed when logged out), with the settings gear moved up next to them.
- An optional setting briefly flashes a tab in the platform's color when one of
  its channels goes live (Appearance).
- Compact chat mode packs message rows tighter to fit more lines on screen
  (Appearance, off by default).
- A mod-only "Mod card ↗" link in the Twitch usercard opens twitch.tv's moderator
  viewer card for that user, and "Open profile" is renamed "Channel".
- Joining a Twitch channel now backfills up to 800 recent lines (Chatterino's
  depth), including replayed subs, raids, and announcements as faded event rows,
  with day dividers marking where a new calendar day starts. Reconnects on both
  Twitch and Kick backfill the messages missed while disconnected.
- The chat-mode bar now shows Kick's chat restrictions too (emote-only,
  sub-only, slow, followers-only), updating live as moderators toggle them.
- Chat search: press Ctrl+R (or right-click a tab → Search) to search that
  tab's buffered chat history in its own window. Results look like real chat
  rows (badges, emotes, name colors) and narrow as you type; clicking one
  jumps the chat log to that message and flashes it.
- Pressing Escape closes the search, settings, and viewer-list windows.

### Fixes

- YouTube standard emoji now show as the emoji character instead of leaking the
  raw `:shortcut:` text.
- Two tabs sharing the same Twitch channel — or editing a tab's Kick/YouTube
  channel while keeping its Twitch one — no longer silently stop that channel's
  Twitch chat.
- Chatters who never picked a name color get their stable Twitch-assigned color
  instead of one shared default, badges no longer vanish for the whole session
  when a badge fetch hiccups, and 7TV name paints apply to the join backlog.
- The events panel no longer shows a stray sub badge next to a sub's attached
  message, and `/slow` explains Twitch's 3–120 second range instead of
  surfacing a raw API error.
- Smoother long sessions: the mentions and events panels no longer rescan the
  whole chat buffer every frame.
- Switching tabs (and opening the app or a popout window) now focuses the
  message box, so typing and shortcuts like Ctrl+R work immediately without
  clicking into the view first.

## v0.4.5

### Features

- Inactive tabs are marked unread (bold, un-dimmed name) when new live chat or
  events arrive, cleared when you select the tab; join-backlog history never
  triggers it.
- The AutoMod held-message chatter name and the pin banner's message author and
  pinning-moderator names are now clickable, opening that user's usercard.
- Messages sent by redeeming Twitch's built-in "Highlight My Message" channel-point
  reward now show as a highlighted row with a HIGHLIGHTED tag, in their own color
  you can set under Settings → Themes on a custom theme.
- Right-click a chatter's name in chat to reply to them: their `@name` is added
  to the message box (with sensible spacing) and the send target switches to
  their platform.
- Usernames in event rows are clickable — the actor, any @mentions, and the
  author and mentions of a sub/resub's attached message all open that user's
  usercard.
- The usercard header has an Open-profile link that opens the chatter's profile
  page on Twitch, Kick, or YouTube.
- The chat-mode bar's placement is now Off / Top / Bottom (defaults to Top)
  instead of a plain on/off toggle.

### Fixes

- Watch-streak rows now show the real streak length from Twitch's data instead of
  accidentally reading digits out of the chatter's username (a viewer like
  `user67` with an 80-stream streak showed 67).
- A failed Twitch unban of a user who wasn't banned now shows as a muted notice
  instead of a red error row.
- Tightened the gap between words in chat so spacing reads evenly across the log,
  mentions, AutoMod rows, event rows, and reply previews.
- Raised the username contrast floor so dim blue/red/purple names that sat just
  below readable on the darkened chat log are nudged legible while keeping their hue.
- Tabs wrap onto multiple rows instead of scrolling horizontally, so every tab
  stays visible at once (Chatterino-style); the scroll strip and arrows are gone.
- The account-wide Twitch emote set is fetched once for the whole app instead of
  once per tab, fixing a flood of rate-limit errors when many empty tabs were
  open. A rate-limit is now told apart from a missing-permission error in the log.
- Chat line height follows the font's real metrics instead of a fixed multiple,
  tightening line spacing with a little breathing room between messages.
- Accounts whose name starts with `@` (like some bots on Kick and YouTube) now
  match ignore / suppress / highlight / mention rules and Kick moderation lookups.
- The settings content area shows a persistent scrollbar when a category runs
  past the bottom of the window, so it's clear there's more below.
- Settings pickers were redesigned: Off/Top/Bottom, streamer mode, and mod-button
  mode are now dropdowns, and the mod-button platform scope and the Text/Regex/User
  add-mode selector are segmented pill controls.

## v0.4.0

### Features

- Per-message mod buttons: a customizable button strip on the left of chat rows
  in channels you moderate. The stock Delete / Ban / Timeout 10m buttons are
  seeded on first run; add your own in Settings → Mod Buttons with any slash
  command or plain chat text (bot commands work) — known commands target the
  row's author or message automatically, and `{user}` / `{msg-id}` placeholders
  are available for custom placement. Buttons can be reordered, edited, scoped
  to one platform, and shown Always, on hover, or not at all. Clicks act on the
  clicked row's platform regardless of the send-target toggle.
- A curated icon set for mod buttons — one lucide icon per mod action (ban,
  delete, warn, pin, monitor, restrict, and four timeout styles plus three warn
  styles for telling multiple buttons of the same kind apart) — or type any
  text/emoji as the button face.
- Custom mod buttons can also be added to the usercard's moderation panel
  (user-targeting commands only), scoped per platform.
- @mentions in chat are clickable: clicking `@name` in any message opens that
  user's usercard on the message's platform, whether or not they've chatted —
  someone we've seen gets a fully filled card right away, anyone else gets a
  card whose account stats load in (or report that no such user exists).
- Pause chat on hover (Settings → Appearance, off by default): the log holds
  still while your pointer is over it — a "Chat paused" pill shows — and jumps
  to the newest messages when you move away. A log you scrolled up yourself
  never pauses or jumps.
- A suppress tier next to ignore (Settings → Highlights): messages matching a
  suppress term stay in chat but render at a configurable low opacity, so
  they're easy to skip while still readable.
- Per-user ignore and suppress: a `user:[platform/]name` term filters a whole
  user (optionally on one platform), with a Text / Regex / User add-mode
  selector in the editor so you don't have to type the syntax.
- The mentions panel jumps to a clicked mention's message in the chat log and
  briefly flashes it; if it has aged out of the buffer, a transient note says so.
- Channel-point redemptions that include a message now show that message under
  the "X redeemed …" notification, with badges and emotes intact.
- Deleting single messages on Kick works again, now through Kick's new official
  API (`/delete` or a delete mod button). Log out and back in on Kick to grant
  the new permission it needs.
- `/timeout` accepts an optional reason after the duration, on both platforms.
- Command aliases (`/untimeout`, `/viewers`, `/user`, …) are listed as their
  own rows in the `/` autocomplete popup so they're discoverable.
- Message timestamps can be hidden independently for the chat log, events
  panel, and mentions panel (Settings → Appearance).
- The chat-mode bar can be moved to the top of the chat panel instead of above
  the message box (Settings → Appearance).
- Tabs redesigned as compact chips: the active tab gets an accent tint and
  underline, inactive ones a recessed fill, and closing a tab moved into its
  right-click menu so it can't be hit by accident.
- The main window's and usercard window's position and size are remembered and
  restored on the next open (when the same display is still connected).
- The pin banner, collapsed-pin chip, and hover pin button use a proper pin
  icon instead of the 📌 emoji.

### Fixes

- Kick mod actions (usercard ban/timeout panel, the mod-button strip, mod-only
  command autocomplete) now show only when you actually moderate the Kick
  channel, checked automatically at connect and login (the broadcaster always
  counts) and refreshed from your own messages' badges — previously any Kick
  login showed them everywhere. No mod on either platform = no button-strip
  space reserved at all.
- Kick pin/unpin is disabled with a clear notice instead of failing with an
  authentication error — Kick's API doesn't offer pinning to third-party apps
  yet (the delete fix above came from Kick adding exactly such an endpoint, so
  pinning will return if Kick does the same).
- Mod buttons whose command can't run on a row's platform (like a Twitch-only
  `/warn` on a Kick message) now gray out instead of failing when clicked.
- Text/emoji mod-button faces are no longer struck through on rows of banned
  users.
- Slash commands taking a user or channel accept a leading `@` (`/unban @name`,
  `/ban`, `/timeout`, `/warn`, `/shoutout`, `/raid`, …) instead of failing to
  find the account.
- A Kick-only tab defaults its send target to Kick, and the send-target toggle
  only appears when the tab has both a Twitch and a Kick channel.
- The live viewer count now appears as soon as a stream goes live, instead of
  sometimes staying blank until the first periodic update.
- Adding an ignore term now cleanly collapses the matching messages already on
  screen, instead of leaving blank gaps where they were.
- The selected theme in Settings → Themes is clearly distinguishable in dark
  mode (accent tint, left accent bar, and a more visible swatch ring).
- Hover effects and tooltips no longer stick around when the mouse leaves the
  window.
- The tab tooltip's uptime/last-seen rolls into days at 24h, so 46h reads
  "1d22h" instead.

## v0.3.0

### Features

- Slash commands: typing `/` opens an autocomplete popup with the commands available
  for the current send target — moderation (`/ban` `/timeout` `/unban` `/delete`
  `/warn` `/clear`), chat modes (`/slow` `/followers` `/subscribers` `/emoteonly`
  `/uniquechat` and their `off` variants), `/announce` (with color variants), `/mod`
  `/vip` `/shoutout` `/raid` `/me` `/usercard` `/chatters`, and a twitch.tv-style
  `/pin [duration] <message>` that sends the message and pins it. Mod-only commands
  are hidden unless you moderate the channel; broadcaster-only ones unless it's your
  channel. Log out and back in on Twitch to grant the new permissions the commands
  need.
- Chat-mode bar: active chat restrictions (followers-only, sub-only, emote-only,
  slow, unique) show as chips above the message box.
- Twitch `/announce` announcements now appear as highlighted announcement rows
  (megaphone header, the announcement's color as the row accent), with an
  Announcements toggle in the events-panel filter.
- Your Twitch sub emotes from other channels now work in autocomplete and are cached
  for an instant warm start (needs a fresh Twitch login).
- Redesigned usercard: fixed header and actions with a scrolling recent-messages
  section, platform icon, and a copyable user ID. Timeouts got preset chips (1s–2w,
  capped at 7d on Kick) plus a custom-duration box; `/timeout` accepts the same
  duration strings ("90s", "1h30m", "3d").
- Redesigned chat surface: deeper dark/light palettes, full-width accent-bar highlights
  for mentions / first messages / events / going-live, a "FIRST MESSAGE" pill,
  browser-style tab chips, and auto-hiding scrollbars.
- The message box moved inside the chat panel, with the send-target toggle and emote
  button built into it; the viewer-list button moved to the new status bar.
- New live status bar above chat: each live platform's icon, channel, and viewer count
  (animated rolling number, plus a Total when more than one platform is live). Toggle
  it in Appearance settings.
- Redesigned settings window: an icon category rail, grouped setting cards with
  switches, and platform-logo account rows. Tab settings got the same layout
  (Channels / Panels / Highlights pages).
- Logging in and out moved to Settings → Account; the /login, /logout, /kicklogin,
  and /kicklogout commands were removed.
- Redesigned events panel: compact rows (kind-colored dot, timestamp, platform logo,
  bold actor + condensed detail), Twitch mass-gift recipients collapsed under one
  expandable announcement, condensed watch-streak rows, gifters' lifetime gift totals
  on mass-gift rows, and per-tab "Hide sub messages" / "Collapse gift batches"
  switches. Kick gift bombs in chat name at most three recipients plus "and N others".
- Pinned messages: floating collapsible banner cards, and pinning/unpinning now
  confirms with a message preview (in popout windows too).
- The tab live tooltip is a compact stream card, so long category names no longer
  overflow it.
- The global Mentions tab can be renamed in Highlights settings.
- Twitch usercards have a Warn section: type a reason and the chatter must
  acknowledge the warning before they can chat again.

### Fixes

- A user who was banned and later unbanned before you joined no longer shows their
  new messages struck through — replayed bans from chat history now only strike
  messages from before the ban.
- Mentions inside a sub/resub's attached message now feed the Mentions tab and the
  alert sound, and mentions are no longer silently missed once the message buffer
  is full.
- `/pin` can pin a message that starts with a number without reading it as a
  duration (start it with `--`).
- Muted mention terms played the alert sound again after an app restart.
- In the autocomplete popup, Tab now inserts the highlighted candidate and Up/Down
  navigate the list; pressing Down with a typed draft (outside history browsing)
  clears it.
- `/followers` reads a bare number as minutes like twitch.tv (`/followers 10` =
  10 minutes, previously seconds) and accepts `0` for no minimum follow age.
- Every announcement color (`/announceblue` `/announcegreen` `/announceorange`
  `/announcepurple`) is listed in the autocomplete popup, and completing a color
  no longer inserts `/announceblue` in its place.

## v0.2.2

- New app logo (window, taskbar, installer, and shortcuts).
- Mention sound alerts (opt-in): enable "Play a sound on mention" in Highlights settings.
  Each term chip (including your account names) has a bell button to mute just that term,
  and streamer mode silences all pings by default (changeable in Streamer Mode settings).
- New About settings category: app version, the beta-updates opt-in, GitHub /
  release-notes links, and an "Open install folder" button.
- After an update applies, a one-time banner announces the new version with a
  "What's new" link; the update banner links to the release notes too.

## v0.2.1

- The app version is shown in the settings sidebar (pre-release builds are marked "beta").
- New option "Get beta updates": opt into pre-release builds; betas move to the
  next stable release automatically.
- The emote image cache moved out of the install directory, and uninstalling now removes it.

## v0.2.0

First installable release.

- Windows installer (`Backseater-win-Setup.exe`) and portable build, published on GitHub Releases.
- Automatic updates: the app checks for new releases in the background and shows a
  "restart to apply" banner.
- The app itself: tabbed multi-platform live chat for Twitch, Kick, and YouTube — merged
  per-tab feeds, 7TV/BTTV/FFZ emotes, badges, events, moderation tools, pinned messages,
  themes, highlights/ignores, streamer mode, and more.
