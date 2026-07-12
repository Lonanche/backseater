# Changelog

Each `## vX.Y.Z` section becomes the GitHub release notes for that version
(extracted by `.github/workflows/ci.yml` when it auto-publishes a release).

## v0.3.0-beta.1

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
- Fixed: muted mention terms played the alert sound again after an app restart.

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
