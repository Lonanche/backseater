# Changelog

Each `## vX.Y.Z` section becomes the GitHub release notes for that version
(extracted by `.github/workflows/ci.yml` when it auto-publishes a release).

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
