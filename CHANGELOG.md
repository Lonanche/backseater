# Changelog

Each `## vX.Y.Z` section becomes the GitHub release notes for that version
(extracted by `.github/workflows/ci.yml` when it auto-publishes a release).

## v0.4.0-beta.2

### Fixes

- Kick mod actions (usercard ban/timeout panel, the mod-button strip, mod-only
  command autocomplete) now show only when you actually moderate the Kick
  channel, checked automatically at connect and login (the broadcaster always
  counts) and refreshed from your own messages' badges — previously any Kick
  login showed them everywhere. No mod on either platform = no button-strip
  space reserved at all.

## v0.4.0-beta.1

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
- Deleting single messages on Kick works again, now through Kick's new official
  API (`/delete` or a delete mod button). Log out and back in on Kick to grant
  the new permission it needs.
- `/timeout` accepts an optional reason after the duration, on both platforms.
- The pin banner, collapsed-pin chip, and hover pin button use a proper pin
  icon instead of the 📌 emoji.

### Fixes

- Kick pin/unpin is disabled with a clear notice instead of failing with an
  authentication error — Kick's API doesn't offer pinning to third-party apps
  yet (the delete fix above came from Kick adding exactly such an endpoint, so
  pinning will return if Kick does the same).
- Mod buttons whose command can't run on a row's platform (like a Twitch-only
  `/warn` on a Kick message) now gray out instead of failing when clicked.
- Text/emoji mod-button faces are no longer struck through on rows of banned
  users.

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
