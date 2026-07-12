# CLAUDE.md — Backseater

Onboarding + working notes for Claude Code sessions on this project. Read this first.

## What this is

A from-scratch **Rust + GPUI** desktop chat client, heavily inspired by **Chatterino2**
and its Kick/YouTube fork **Mergerino** (C++/Qt). This is **not a port** — no code is
shared; ideas, protocol knowledge, and behaviors were studied from them (credited in
README.md). Goal: a clean, expandable multi-platform live-chat client where **adding a
platform = implement one trait + one message builder, with zero UI changes**.

- GUI framework: **GPUI** (the engine behind the Zed editor) + **gpui-component** (longbridge widget kit).
- Note: GPUI is by the Zed team; egui (a different framework) is by emilk — don't confuse them.

## Current status (verified working)

**Working and tested live (tabbed, per-tab merged feed):**
- **Tabs**: a tab strip on top of the window; each tab is an independent channel set (its own
  Twitch and/or Kick and/or YouTube channel, feed, send target). Add (`+`) / close (`✕`) /
  left-click to select / right-click → settings (name + Twitch/Kick/YouTube channels) or
  **Open in new window** (a popout: a second `ChatView` on the same shared channel model —
  `popout.rs`, a mirror, not a move). Tabs persist to
  `<config>/backseater/tabs.json` and restore on launch. Login is **app-wide** (shared by all tabs).
- **Shared channel model** (`app/src/channel_store.rs`): a `ChannelModel` entity owns the row
  buffer + connection + per-channel state, keyed by `ChannelKey`; every view (tab or popout) on
  the same channel shares one buffer/connection and reconciles its own `ListState` from granular
  `ChannelEvent`s. Ignore/mention filtering is per-view at render.
- **Shared Twitch IRC** (`twitch/src/irc_manager.rs`): one read + one write IRC connection for
  the whole app regardless of tab count; `TwitchSource` is a thin handle that registers its
  channel onto the shared sockets. JOINs go through a leaky-bucket limiter (18/12.5s).
- Anonymous **Twitch** chat (tmi IRC) + anonymous **Kick** chat (Pusher WebSocket), interleaved
  in one log per tab, each message tagged by its platform.
- **Native Twitch emotes** + **inline Kick emotes** (`[emote:id:name]`) rendered inline.
- **7TV emotes** on Twitch (global + per-channel), animated ones play (WEBP variant; see render notes).
- Colored usernames, timestamps, system notices, clear-chat/timeout notices.
- **Public channel events** (Twitch + Kick, anonymous — they're public): shown as highlighted rows.
  Twitch uses tmi's ready-made `USERNOTICE` `system-msg` (covers sub/resub/gift/mystery/raid/
  announcement/ritual/bits in one accessor). Kick formats Pusher events: `SubscriptionEvent`,
  `GiftedSubscriptionsEvent`, `StreamHostEvent` (Kick's "raid"/host), `KicksGifted` (bits/cheer
  equivalent), `RewardRedeemedEvent` (channel points). Kick has **no** raid event — host is its
  equivalent. (Leaderboard/prediction Pusher events are intentionally ignored;
  pinned-message events feed the pinned banner — see the pinned-messages bullet below.) A sub/resub's attached chat message is carried on the event as a full `Message`
  (`ChatEvent::Event { message: Option<Box<Message>>, .. }`, author + badges + timestamp + tokens)
  and rendered *under* the system text as a normal-looking chat line (timestamp, badges, colored
  name, emotes inline — Twitch-web style), not flattened into the system text — see
  `render::render_event`/`event_message_line`. Events also carry a `timestamp`, shown in the
  events pane (the chat log omits it).
- **Per-message platform icon** before the timestamp (Twitch logo as a small **bundled PNG**
  via `Platform::icon_url` → `crates/app/assets/twitch/twitch.png`, served by `assets.rs`;
  brand-colored glyph fallback for platforms without a logo). ⚠️ Do NOT point `icon_url` at a
  remote/large **SVG**: gpui's `img()` SVG path rasterizes at the SVG's *intrinsic* size × 2
  and never frees it — the Twitch logo (2400×2800 viewBox) became a **105 MB** decoded bitmap
  for a 16px icon. Bundle a small raster instead (~14 KB decoded).
  **Row alignment is font-metrics-based** (Chatterino-style): `main.rs::apply_font` publishes the
  active font's per-em ascent/descent/cap-height (`render::set_font_metrics`, atomics like
  `is_dark_theme`); `Scale` derives the line box's real text baseline from them. Timestamps render
  at the **chat font size** (like Chatterino's `TimestampMedium` — a smaller size can't share both
  the text baseline and the image centers), and icons/badges sit in `render::image_line_box`, which
  centers the image on the text's *optical* center (baseline − cap/2, rounded to whole px —
  geometric `items_center` sat images ~1px off the glyphs and on fractional pixels).
- **Twitch sub/VIP/mod/etc. badges** rendered before the username (no auth — see badge note below).
- **Kick standard badges** (mod/vip/og/founder/staff/broadcaster/verified/...) rendered before the
  username from bundled images (`crates/app/assets/kick/badges/`).
- **Kick subscriber badges** (per-tier images) — resolved at channel join from the Cloudflare-fronted
  channels endpoint (fetched in-process via the `wreq` browser-TLS client), then matched to each
  chatter's month count. See the Kick note below.
- **Twitch + Kick login** (OAuth) → **send messages** + **moderation** (`/ban` `/timeout` `/unban`
  `/delete`) via the input box and `/commands`. A ban/timeout strikes through + fades the target's
  past messages (kept struck after `/unban`). A send-target toggle (shows the target **platform
  icon(s)** — Twitch / Kick / both — with a tooltip) appears when logged into both; mod commands are
  disabled in Both mode. See the auth note below.
- **Twitch moderator feed** (EventSub, when logged in + mod/broadcaster of the channel): rich
  moderation notices with the acting moderator ("mod timed out user for 10m: reason" — also unbans,
  deletes, warns, chat-mode toggles, ...) replacing the generic "X was timed out / banned" line, and
  **AutoMod**: held messages appear as amber rows with **Allow/Deny** buttons, resolved in place when
  any mod acts. Single deleted messages (IRC `CLEARMSG`) strike just that row for everyone. Needs the
  scopes added for it — an older login keeps working, the feed just stays off until the next `/login`.
  See the moderator-feed note below.
- **Pinned messages (Twitch + Kick)**: a banner above the log shows each platform's active mod
  pin ("📌 Pinned by X" + the message rendered like a chat line), with an ✕ that dismisses *just
  that pin* locally (session-only, keyed by message id) and per-platform "Show pinned messages"
  checkboxes in Appearance settings (persisted; process-wide flags via
  `settings::show_pinned(platform)`). Receiving is anonymous: Twitch pins ride the existing Hermes
  socket (`pinned-chat-updates-v1.<channel_id>`, `pin-message`/`update-message`/`unpin-message` —
  parsed leniently, unrecognized payloads logged at debug); Kick pins are the Pusher
  `PinnedMessageCreatedEvent`/`PinnedMessageDeletedEvent` (previously in the ignored bucket; the
  event's `message` is a full `ChatMessageEvent` shape → the normal `build_message`, `duration` is
  seconds-as-string). On join the current pin is seeded: Kick via the anonymous
  `GET kick.com/api/v2/channels/{slug}/pinned-message` (wreq, tolerant parse), Twitch via Helix
  `GET /chat/pins` (only when logged in — it's moderator-gated; 403 = not a mod = no seed).
  **Mods pin/unpin**: a hover 📌 button on chat rows (next to ↩ reply, shown when the user can
  moderate that row's platform: Twitch `twitch_mod`, Kick = logged in) and an Unpin button on the
  banner. Twitch uses the official Helix `PUT`/`DELETE /helix/chat/pins` (query params only:
  broadcaster_id, moderator_id, message_id, duration_seconds 30–1800 or omit = until stream ends;
  covered by the `moderator:manage:chat_messages` scope we already request — no re-login needed;
  **verified live**, 204s). The 📌 button is hidden on our own sent messages' *local echo* rows —
  their synthetic `echo-…` id (IRC doesn't echo PRIVMSGs) is rejected by Helix with a 400
  (`ChatView::can_pin_row`). Kick has **no public pin API** — we call the site's `POST`/`DELETE
  /api/v2/channels/{slug}/pinned-message` via wreq with the OAuth bearer, rebuilding the message
  object from our `Message` (`chat_id` = the v2 `chatroom.channel_id`, same id history keys on).
  That route sits behind Laravel's `web` middleware (a first live test 419'd "CSRF token
  mismatch"), so the shared wreq client keeps a **cookie jar** and writes send an `x-xsrf-token`
  header (`api::csrf_token`: the `XSRF-TOKEN` cookie percent-decoded, primed by a channel-page GET
  when the jar is empty; the session cookie rides the jar). ⚠️ Still **unverified** whether Kick
  then authorizes a public-API OAuth token there (the web client sends its session token) — a
  rejection surfaces as an error row. Timed pins expire client-side
  (`ChatView::schedule_pin_expiry` + render-time check); a pin with no expiry stays until unpinned.
- **Twitch viewer list** (👥 button on the input bar, or `/chatters`/`/viewers`): a child OS
  window listing who's connected to the tab's Twitch chat, with a live search filter, count,
  Refresh, and click-a-name → usercard. Data is Helix `GET /chat/chatters` (paginated,
  `crates/twitch/src/helix.rs::chatters`), which Twitch only serves to the **broadcaster +
  moderators** (`moderator:read:chatters`, added to `SCOPES` — a pre-existing login needs a
  re-`/login`, surfaced as a 401 hint; a non-mod gets the explanatory 403 message). ⚠️ The
  *anonymous* list twitch.tv itself shows rides the web GQL `chatters` field, which is behind
  Twitch's browser-integrity check (`IntegrityCheckFailed` for any third-party client — verified
  live; same reason Chatterino's viewer list is mod-only). **Kick has no chatters API at all**, so
  the button only appears when the tab has a Twitch channel. The window body isn't virtualized:
  display is capped at `viewerlist::MAX_SHOWN` (500) names with an "…and N more — search to
  narrow" footer. The search `InputState` is **window-bound** (created against the viewer-list
  window when it opens, like the settings inputs); list state lives on `ChatView`
  (`viewer_list`/`viewer_list_window`), module `crates/app/src/viewerlist.rs`.
- **Live status bar + viewer counts**: a slim bar at the top of each chat view (above the pin
  banners; popouts get it too) shows, per live platform of the tab, its icon + channel name + a
  live dot + the concurrent viewer count (`ChatView::render_status_bar`; hidden when nothing is
  live); the tab-chip tooltip shows the same count next to the uptime. Counts ride a dedicated
  `ChatEvent::Viewers { platform, count }` (separate from `Live` so a count refresh can't clobber
  title/game/last-stream state), stored in `ChannelModel.viewer_counts` and cleared on an offline
  `Live`. Optional: Appearance → "Show live status bar" (`Settings.show_status_bar`, default on,
  process-wide flag `settings::show_status_bar()` like the pinned toggles;
  `Settings::apply_visibility_flags` publishes all three). When 2+ platforms have counts, a
  **Total** segment (sum of the displayed values) closes the bar. Counts are **deduped +
  live-gated in the store** (`ChannelModel`'s `Viewers` arm: an unchanged count is dropped, a
  `Some` for an offline platform is a late frame racing the offline clear and is dropped too)
  and fan out as their own granular `ChannelEvent::ViewersChanged`, which views answer with a
  bare `cx.notify()` — NOT `Changed`, whose handler does a full `list_state.reset()` +
  `refresh_log()` (that was a review finding: counts arrive every ~30s, they must never
  re-measure the log). Sources: **Twitch** = the Hermes socket's
  `video-playback-by-id.<channel_id>` topic (`twitch/src/pubsub.rs`, a third subscription next
  to points/pins): anonymous `viewcount` pushes every ~30s while live — the exact number
  twitch.tv shows (`stream-down` is deliberately NOT mapped to a clear: it also fires on
  mid-stream ad transitions; a real offline is cleared by the poll's `Live{live:false}`); a GQL
  `stream{viewersCount}` **seed** (`twitch/src/viewers.rs`, fired from the bridge's live poll)
  fills the gap until the first push — it only emits and only latches `seeded` on a real
  number (GQL lags IVR at go-live; forwarding its `None` deleted a pushed count), retrying each
  30s poll until one lands. ⚠️ Don't poll the count: IVR caches `/twitch/user` for minutes and
  even GQL only moves in coarse (~1min+) buckets — both verified live frozen while the Hermes
  push moved (that's why GQL is seed-only). **Kick** = a poll of the light
  `api/v2/channels/{slug}/livestream` endpoint (wreq, `bks_kick::fetch_viewer_count`): 30s
  while live, backed off to 120s while offline (`KICK_OFFLINE_POLL_SECS` — going live is
  Pusher-push, only the first count waits); live/offline *transitions* stay Pusher-push.
  `data: null` = offline; the count field is **`viewers`**, not the channel endpoint's
  `viewer_count` (verified live, both accepted), and a live response *missing* the count is an
  error → the poll keeps the previous number instead of blanking a live stream. **YouTube** = a
  throttled 30s InnerTube `updated_metadata` call riding the chat poll loop
  (`updateViewershipAction`'s "N watching now"): a failed *request* keeps the previous count, a
  response with no viewership clears it (else a stale number freezes on screen). Each
  fetched/pushed count is `debug!`-logged with its channel. The bar's number doesn't snap: it
  counts up/down to each fresh value Twitch-style (`ViewerAnim`/`eased_count` in `chatview.rs`,
  900ms ease-out, repainting on a coalesced 50ms per-view timer that a settled `from == to`
  anim never arms — the log stays cached, only the chrome repaints; the first count and a
  platform re-appearing show as-is).
- **Chat-mode bar (Twitch today, multi-platform by design)**: a slim bar directly above the
  composer's input row shows each platform's active chat restrictions as chips — platform icon +
  "Followers-only (10m)", "Sub-only", "Emote-only", "Slow (5s)", "Unique" — hidden entirely when
  nothing is restricted (the common case). The seam is generic: `bks_platform::ChatModes` is a
  platform-agnostic struct and `ChatEvent::ChatModes { platform, modes }` always carries a **full
  snapshot** (a connector whose platform sends deltas merges them itself), stored per-platform in
  `ChannelModel.chat_modes` (deduped; empty modes remove the entry) and fanned out as
  `ChannelEvent::ChatModesChanged`, which views answer with a bare repaint — NOT `Changed` (no log
  re-measure), same rule as `ViewersChanged`. Rendered by `ChatView::render_mode_bar`. Twitch
  feeds it anonymously from IRC ROOMSTATE (`irc_manager.rs::merge_roomstate` + per-`Channel`
  `modes`/`modes_synced` state): the join ROOMSTATE carries every tag, later ones only the changed
  tag (tmi maps missing → `None` = no change); `followers-only` -1/0/N maps to off / zero minimum
  / N minutes, `slow=0` is off; the first ROOMSTATE of each session re-emits unconditionally so a
  mode flipped while disconnected can't leave the bar stale (the store's dedupe eats the no-op
  case). **Kick later**: emit `ChatModes` from the Pusher `ChatroomUpdatedEvent`
  (slow/subscribers/followers/emotes modes; currently in the ignored bucket) + seed from the
  channel lookup's `chatroom` fields — zero UI changes needed. Durations format via
  `bks_core::format_duration` (compact "1h30m", the inverse of `parse_duration`).
- **Dark + light + custom themes**, switched live in the Appearance/Themes settings tabs
  (persisted in `settings.json`). The kit chrome switches via `gpui_component::Theme::change`; the chat
  log's own colors come from a `render::palette()` (`DARK`/`LIGHT`) selected by the process-wide
  `bks_core::is_dark_theme()` flag (set from `main.rs::apply_theme`, mirroring the `preferred_scale`
  pattern). `readable_color` adapts too: it lightens dark names on a dark bg and darkens bright names on
  a light bg. **Custom themes** (Themes settings category, kit `ColorPicker`s): user-saved color
  profiles — `ThemeChoice::Custom(name)` + `Settings.custom_themes`, applied via
  `render::set_custom_palette` (`Palette::from_custom`); `apply_theme` is split so live edits work
  without a window.
- **Font family setting** (Appearance → "Font"): a searchable kit `Combobox` listing every installed
  font (`cx.text_system().all_font_names()`) plus a "Default (system)" entry; persisted as
  `Settings.font_family: Option<String>` (`None` = system default) and applied app-wide via
  `main.rs::apply_font` → `gpui_component::Theme.font_family` (the kit `Root` sets it on every
  window's root div, so chat inherits it — `render.rs` sets no family of its own). The
  `ComboboxState` is window-bound like the kit inputs, so it's recreated in `rebind_settings_inputs`
  (subscription replaced with it); a change re-measures every tab's log (`ChatView::remeasure`,
  glyph metrics change row heights), and `set_theme` re-asserts the font after `Theme::change`.
- **Status notices are not shown in chat** — **errors** and **moderation notices** are. A
  connector/session error becomes `ChatEvent::Error` → a `Row::Error` rendered by `render::render_error`
  as a red-tinted, **selectable + copyable** row (a "⧉ copy" button puts the full text on the clipboard,
  for bug reports). Moderation outcomes the user should see in chat (Kick's rich ban/timeout/unban/
  deletion notices) are `ChatEvent::Notice` → a muted `Row::System` row (Twitch's generic "X was timed
  out / banned" is pushed directly by the ClearChat arm). Status chatter ("logged in…", "sending to: X",
  emotes loaded, …) is `tracing::info!`-logged instead.
- **Streamer mode** (`app/src/streamer_mode.rs`): hides privacy-sensitive UI while live — currently
  usercard avatars render as a 🕶 placeholder, click to reveal (per-card `avatar_revealed`). Active
  state is a process-wide flag (`streamer_mode::is_active()`, same pattern as `is_dark_theme`);
  driven by a persisted three-way setting (`Settings.streamer_mode`: Off / On / **Auto**, default
  Auto) in its own "Streamer Mode" settings category. Auto = on while broadcast software runs
  (OBS/Streamlabs/XSplit/Twitch Studio/vMix/PRISM — Chatterino's list), detected by polling the
  process list via a Toolhelp snapshot (`windows` crate, cfg(windows)) every 10s on the background
  executor (each poll logged at `debug!`), plus one synchronous check at launch. `BackseaterApp::apply_streamer_mode` recomputes
  the flag and notifies every tab (usercards render against their tab's ChatView) — don't set the
  flag anywhere else. While active, a warning-tinted banner under the tab strip says so, with a
  "Turn off" button (sets the setting to Off) and an ✕ that dismisses just the notice
  (`streamer_banner_dismissed`, session-only, reset on each activation).
- **Highlights + ignores** (Highlights settings category): per-user/word highlight terms and
  ignore terms (word, phrase, or `re:<regex>`), global and per-tab; a **mentions panel** shows
  messages that mention the user (per-tab or all-tabs feed with "#channel" tags, click → jump to
  the source tab).
- **Mention alert sound** (`app/src/sound.rs`): a bundled synthesized ping
  (`crates/app/assets/sounds/ping.wav` — ours, NOT copied from Chatterino; its wav's provenance
  is undocumented) played via Win32 `PlaySound` (`SND_MEMORY|SND_ASYNC`, `windows` feature
  `Win32_Media_Audio`). **Opt-in**: Highlights → "Play a sound on mention" (default off);
  a per-term bell toggle on every mention chip (vector icons: the kit's lucide `bell` + our
  bundled `icons/bell-off.svg` — emoji 🔔/🔕 rendered ambiguously at chip size) — account names
  show as fixed, deduplicated "(you)" chips so they're muteable too — backed by one app-wide
  `Settings.muted_mentions` list keyed on `bks_core::normalize_term`; Streamer Mode → "Mute
  mention sounds while active" (default on).
  A muted term still *highlights* (only the sound differs): `MentionMatcher::with_sound` carries
  per-term flags, `sound_for()` gives the verdict, which rides `MentionEntry.sound` and plays
  once app-wide at `MentionStore::push` (post-dedup); the master/streamer gates are process-wide
  flags (`settings::apply_sound_flags`, read at play time).
- **About settings category + update notices**: About shows the running version
  (`updater::version_label()` — the installed Velopack package version, "-beta" marks the
  channel, "(dev)" a non-installed run; NOT shown in window titles or chat chrome), GitHub /
  release-notes links (`cx.open_url`), and "Open install folder" (`cx.reveal_path` on
  `current_exe`). The update banner has a "What's new" link to the pending release's notes, and
  the first launch after an update shows a one-time success-tinted "Updated to vX — What's new"
  banner (Velopack `on_restarted` hook → `updater::just_updated_to()`, session-only, ✕ clears).
- **Settings** (category sidebar: Account / Appearance / Themes / Highlights / Streamer Mode) and the **usercard** open as
  **separate OS windows** (`child_window.rs`: a `ChildWindow` renders a host entity's body in its own
  `cx.open_window`), draggable off the main window and freely resizable. All panel state stays on the
  host (app / tab view); the window is just a surface. ⚠️ Two rules: kit `InputState`s are
  **window-bound** (focus/blur/cursor subscriptions), so the settings inputs are recreated against the
  settings window each open (`rebind_settings_inputs`) and tab rebuilds go through `main_window.update`;
  and opening a window draws it synchronously (its render re-enters the host), so `child_window::open`
  must run from a plain `App` context (spawned task), never inside the host's own update/listener.
  A third rule: `child_window::open` takes the parent window's **display id** — gpui-windows
  validates requested bounds against the creation display (primary monitor if none given) and
  silently swaps them for that display's big `default_bounds()` when the bounds' center is
  elsewhere, which broke opening over a chat window on a secondary monitor.
  Closing the main window quits the app (`on_window_closed` → `quit`) so child windows don't orphan.
  The **usercard** window is bare (`open_centered_bare`): header + moderation panel stay fixed and only
  the recent-messages section scrolls. Its moderation panel has Chatterino-style preset timeout chips
  (1s → 2w, filtered to the platform's cap — Helix rejects > 2 weeks, Kick's ban API > 7 days;
  `chatview.rs::max_timeout_secs`) plus a **custom-duration box** (a window-bound `InputState` like the
  viewer-list search, created in `show_usercard_window`; Enter or its Timeout button applies, bad input /
  over-cap shows an inline error). Durations parse via `bks_core::parse_duration` ("600", "90s",
  "1h30m", "3d", "1w"); `/timeout` accepts the same strings.
- The **active tab is persisted** (`tabs::save_active`/`load_active`, a separate `active_tab` store) and
  restored on launch, clamped in case that tab is gone.
- **7TV / BTTV / FFZ** emotes on Twitch (all three providers registered in `bridge.rs::providers()`),
  **7TV on Kick** (`kick_providers()`) and **7TV on YouTube** (`youtube_providers()`), animated ones play.
- **Anonymous YouTube live chat** (`bks-youtube`, read-only) interleaved in the same feed. A tab's
  YouTube source is an `@handle` / channel URL / `/live` URL / `watch?v=` / bare video id; the connector
  resolves the *currently live* video and follows it. Reads use YouTube's private **InnerTube** web API
  (no key, no OAuth, **no quota** — the same one youtube.com uses): scrape `INNERTUBE_API_KEY`/
  `CLIENT_VERSION`/`visitorData` from a bootstrap page (`api.rs`), POST `youtubei/v1/next` for the initial
  live-chat continuation, then long-poll `live_chat/get_live_chat` honoring each response's `timeoutMs`
  (`connector.rs`). Renderers → `Message`/`Event` in `builder.rs`: text + **custom channel emojis** inline
  (`isCustomEmoji` → `Emote`; unicode emoji stay text), author name color / member-green, **membership
  badges** (custom-thumbnail), and **Super Chats / new members / gifted memberships** as highlighted event
  rows (`Bits`/`Sub`/`Gift`). Send/moderation are **not** built (see below).
- **Mentions + links** parsed and rendered: `@name` mentions tint/emit `MessageElement::Mention`
  (`core/mention.rs`), URLs become clickable links (`core/links.rs`) — a 7TV emote link opens an
  in-app emote popup, other links confirm-then-open.
- **Badge tooltips** on Twitch and Kick (hover a badge → larger preview + title;
  Kick titles from `kick_badge_title` in `kick/src/builder.rs`).
- **7TV cosmetics**: animated name **paints** (per-char gradient) + 7TV badges, by `(platform, user_id)`.
- **App icon + window title.** `crates/app/resources/icon.ico` (generated from `icon.png` by
  `make_ico.ps1`, 7 sizes, PNG-compressed frames) is embedded by `build.rs` via `winresource` as
  icon resource id **"1"** — gpui_windows' `load_icon` looks up ordinal 1 for the window-class icon
  (title bar + taskbar), and Explorer shows it on the exe; don't change the id. The main window
  title is "Backseater - {active tab}", set in `BackseaterApp::render` (memoized in `window_title`,
  so select/rename/close/restore are all covered with no per-call-site hooks).
- **Windows installer + auto-update (Velopack)**: `.github/workflows/ci.yml` **auto-releases** —
  on every push to main (after clippy + tests pass) it checks whether the workspace version in
  `Cargo.toml` has a GitHub release yet; if not it packs the exe with the `vpk` CLI
  (`Backseater-win-Setup.exe` + portable zip + delta packages) and publishes to **GitHub
  Releases** (the `vX.Y.Z` tag is created by the publish — never push tags by hand). The in-app
  updater (`app/src/updater.rs`) reads the same feed via the `velopack` crate's `GithubSource`. `updater::startup()` (`VelopackApp::build().run()`) is the **first line
  of `main`** — its install/update hooks may exit/restart the process. The check runs at launch +
  every 4h (blocking, on the background executor), downloads silently, then an info-tinted banner
  under the tab strip says "Update X is ready — restart to apply" (Restart →
  `apply_updates_and_restart`; ✕ dismisses session-only — Velopack applies the pending update on
  the next launch anyway). A `cargo run`/portable copy is not a Velopack install →
  `UpdateManager::new` errors → updater quietly off (logged at debug). ⚠️ The crate's
  `UpdateInfo`/`VelopackAsset` fields are C#-style PascalCase (`update.TargetFullRelease.Version`),
  and `UpdateCheck::UpdateAvailable` carries a `Box<UpdateInfo>`. Releases ship **unsigned**;
  release steps are in `docs/RELEASING.md` (bump the version + changelog, push — that's all). A
  `-beta`-suffixed **version** (`0.3.0-beta.1` in Cargo.toml) publishes as a GitHub
  **pre-release**: only users with About → "Get beta updates" (`Settings.beta_updates` →
  `updater::set_beta_updates` → `GithubSource(prerelease)`) receive it, and semver moves them
  back onto the next stable. CI runs in ~5 min warm (`rust-cache`, `shared-key: build`); a
  version bump rewrites Cargo.lock → cache re-key → that one run takes ~15 min (expected, once
  per release).
- 240 passing unit tests (`cargo test`).

**Not done yet (designed for, not built):**
- TikTok connector.
- **YouTube send / moderation.** Reads are done (anonymous InnerTube, above); sending + ban/timeout/delete
  need the **Data API v3** (quota-limited: ~2000 reads/day, which is why *reads* use InnerTube) + **Google
  OAuth** (`youtube.force-ssl`). Google requires a client **secret**, which we won't ship in the binary —
  so, like Kick, it needs a **broker Worker** to hold the secret plus a Google Cloud OAuth app
  (we won't ship an obfuscated secret in the binary).
- Kick **delete-message** (the public API has no endpoint) — `/delete` is Twitch-only.
- BTTV / FFZ emotes on **Kick** (Kick gets 7TV + native only; the seam supports adding them).

## Architecture

```
crates/
  core/      # platform-agnostic domain model. NO gui, NO networking.
  platform/  # the expandability seam: ChatSource trait + ChatEvent (moderation is per-platform types)
  twitch/    # ChatSource impl (shared IRC via irc_manager) + Helix/EventSub/badges/history
  kick/      # ChatSource impl (anonymous Pusher WebSocket) + chat-event builder (+ tests)
  youtube/   # anonymous InnerTube live-chat reader (read-only)
  emotes/    # EmoteRegistry + EmoteProvider trait (7TV / BTTV / FFZ)
  auth/      # OAuth flows (Twitch implicit, Kick PKCE+broker) + stores (keyring for
             #   credentials on Windows, JSON files for app data)
  app/       # GPUI binary: tokio<->GPUI bridge, flex-wrap token rendering
             #   main.rs = BackseaterApp (root view: tab strip, settings, window)
             #   chatview.rs = ChatView (one view's chrome/input/usercard/reply)
             #   channel_store.rs = ChannelModel (shared per-channel row buffer +
             #     connection + state; views observe + reconcile their ListState)
             #   popout.rs = popped-out chat windows (mirror views)
             #   chatview/log.rs = the chat-log region as a CACHED child view
             #     (LogView): picker animation ticks dirty ChatView (gpui dirties
             #     all ancestors of a notified view) but reuse the log's cached
             #     paint. Log changes must go through ChatView::refresh_log.
             #   chatview/picker.rs = the emote picker (grid/tabs/search/EmoteCell
             #     cached views; cells render a cheap poster:// first-frame
             #     thumbnail with the animated img overlaid — grid kept short +
             #     tiny overdraw + 1-thread decode cap bound the always-animated
             #     cost)
             #   render.rs = token rendering (RenderCtx threads row ids + selection)
```

**The seam (read `crates/core/src/message.rs` + `crates/platform/src/lib.rs`):**
- `Message` carries a `Vec<MessageElement>` (Text / Emote / Badge / Mention / Link). This is the
  *only* contract between connectors and the UI. The UI maps each element to a GPUI element.
- `ChatEvent` is what a connector emits: `Message`, `System` (notice), `Event { platform, text }`
  (public sub/gift/raid → highlighted row), `ClearChat`, and
  `Channel(ChannelMeta)`. `ChannelMeta { platform, id, name }` is the connector handing over
  channel identity *generically* — no platform-specific id (like Twitch room-id) leaks through
  the enum. The bridge uses it to fetch per-channel emotes, then drops it (never shown in chat).
- `ChatSource` trait: `join` (live) + `send`. Moderation is **not** a trait — each platform has a
  concrete actions type (`TwitchActions` via Helix REST, `KickActions` via Kick's REST), since their
  request shapes differ too much to share one interface (Twitch keys on channel+login, Kick on numeric
  broadcaster+target ids). The controller dispatches to whichever the tab's send target selects.
- A new platform implements `ChatSource`, produces `Message`s, and emits one `Channel(ChannelMeta)`
  once its connection is live. Nothing in `app/` changes.

**Emote provider seam (`crates/emotes`):** `EmoteProvider` (async: `name` + `load_global` +
`load_channel`) is one source of 3rd-party emotes. `EmoteRegistry::load_providers` merges a
`Vec<Box<dyn EmoteProvider>>` (earlier providers win name collisions) and `resolve_elements`
rewrites a message's `Text` runs into `Emote` tokens. **Adding BTTV/FFZ = implement the trait and
push it into `bridge.rs::providers()` — no other file changes.** `SeventvProvider` is the reference impl.

**Shared globals + interned emotes (Chatterino-style).** Two layers keep multi-tab startup cheap:
- **Globals load once app-wide.** The FFZ/BTTV/7TV *global* sets are identical for every tab, so the
  bridge loads them once into a process-wide `Arc<EmoteMap>` per platform (`twitch_globals()`/
  `kick_globals()`, behind a `tokio::sync::OnceCell`) and each tab's `EmoteRegistry::with_globals`
  *points* at that `Arc` instead of copying it. A per-tab registry only owns its channel emotes.
  `load_globals` logs each set's size **once** (not per tab — that was the startup log spam); per-tab
  channel loads log at `debug!`. (The per-provider URL cache still dedups the network; this dedups the
  in-memory copies + the logging.)
- **Emotes are `Arc`-interned.** `MessageElement::Emote(Arc<Emote>)` (mirrors Chatterino's `EmotePtr =
  shared_ptr<const Emote>`), so resolving an emote into a message and the virtualized log re-cloning a
  visible emote into its click closure *every frame* are pointer bumps, not copies of three `String`s.
  The picker payload (`ChatEvent::Emotes`) stays owned `Vec<Emote>` — it's a one-per-channel snapshot,
  not a per-frame path (`owned_emotes` unwraps at that boundary). Serde's `rc` feature is enabled so
  `Arc<Emote>` still (de)serializes.

**Tabs (`app/src/main.rs`, `chatview.rs`, `tabs.rs`).** `BackseaterApp` (in `main.rs`) is the root
view; each tab's `ChatView` lives in `chatview.rs`. `BackseaterApp` owns a `Vec<TabEntry>`
(each a `TabConfig` + an `Entity<ChatView>`), an `active` index, the shared `Session`, and the
settings-dialog input entities. The tab strip is hand-rolled (`h_flex` of clickable chips, not the
kit's action-based `TabBar`): left-click selects, right-click opens the settings `Dialog`
(`WindowExt::open_dialog`) with name + Twitch/Kick channel fields, `✕` closes, `+` adds. Changing a
tab's channels rebuilds its `ChatView` on a fresh `bridge::connect`. `TabConfig {name,
twitch_channel, kick_channel}` persists to `<config>/backseater/tabs.json` (via `bks_auth::store`).
A tab with no channel set renders a "right-click → Settings" prompt instead of an empty log.

**Data flow (per-tab merged feed):** each tab calls `bridge::connect(session, twitch_ch, kick_ch)`,
which spawns **one tokio task per non-empty platform**, all feeding that tab's *own* smol channel:
- Twitch: owned by the tab's `Controller` (`start()` → `run_twitch`), which resolves 7TV emotes (on
  `Channel`, loads providers into an `EmoteRegistry`; on `Message`, rewrites text runs) + badges. On
  `Channel` it fetches emotes, badges, and the history backlog **concurrently** (`tokio::join!`) so the
  slowest one — not their sum — gates when Twitch history appears; the backlog is then resolved with
  the loaded emotes/badges and emitted oldest-first.
- Kick: `KickSource::join` → `ChatEvent`s forwarded as-is (Kick emotes are already inline-parsed);
  Kick history is fetched in its own spawned task (doesn't block the live read loop). Both platforms'
  history is `historical`-flagged and interleaved by timestamp in the UI (`ChannelModel::insert_message`),
  so they appear merged regardless of which lands first. History backfills **chat only**: replayed
  USERNOTICEs (subs/raids) are dropped and a replayed CLEARCHAT is a `historical` fade (strikes the
  backlog, **no notice row**) — event/notice rows show no timestamp in the log, so a days-old timeout
  or sub would misread as having just happened on every launch (`twitch/src/history.rs`).
Both → the channel's **smol channel** (GPUI-friendly) → the shared `ChannelModel` drains it in
`cx.spawn` (coalescing a burst: `recv()` then `try_recv()` the rest, one notify per burst), pushes
into its ring buffer and emits granular `ChannelEvent`s; each observing `ChatView` reconciles its
`ListState` and `render::render_message` turns tokens into a `h_flex().flex_wrap()` of words +
emotes, prefixed by the platform icon. **Adding a platform = spawn one more task in `connect()`.**

**The log is virtualized** (`gpui::list` + a `ListState` on `ChatView`, `ListAlignment::Bottom` +
`FollowMode::Tail`): only on-screen rows are built per frame (a SumTree caches item heights), which is
what keeps a fast-animating feed smooth while dragging the window (production apps don't re-layout the
whole log every frame; Zed's editor uses the same element). The previous `RENDER_TAIL`/`ScrollHandle`
+ manual `is_at_bottom`/`scroll_to_bottom` are gone — `Tail` mode gives the bottom-stick-unless-reading
behavior natively. **`rows` and `list_state` must never drift**: all structural mutations go through
the model's `row_push_back`/`row_insert`/`row_pop_front` (`channel_store.rs`, each emitting the
`ChannelEvent` every view answers with the matching `ListState::splice`); height-changing
config (font size, events-pane show/resize) calls `list_state.reset(len)` to re-measure. Selection
ordinals are now derived from `row_index * ORDINAL_STRIDE` (not a per-frame running counter) so they
stay globally stable as the visible window shifts (`selectable.rs` only needs ordering, gaps are fine).
One intended consequence: copying a selection captures only rows that were on-screen during the drag
(standard for a virtualized log).

**The events panel is virtualized too** (its own `ListState` on `ChatView`, Bottom + Tail): the
retained buffer holds up to `MAX_EVENTS` (1000) rows for the session, and the old plain scroll
column rebuilt + laid out *all* of them every frame (a scroll div lays out offscreen children —
only paint is clipped), each animated emote in them spawning animation wakeups and pinning its
images in the LRU cache; sessions got choppier as events accumulated. Views track the shared
buffer by **stable sequence numbers** (`ChannelModel::events_base` + index; `event_at(seq)`),
reconciling their per-view filtered `events_shown` list from the model's granular
`EventAppended { seq }`/`EventsTrimmed` events (same lockstep rule as the log); a filter change or
reconnect rebuilds it wholesale (`rebuild_events_shown`). The mentions panel is still a plain
scroll column (bounded: it filters the `MAX_ROWS` ring).

**Emote image cache eviction (Chatterino's `ImageExpirationPool`).** gpui's *global* asset cache
(`window.use_asset`, the default when an `img()` has no scoped cache) **never evicts** — every distinct
image URL ever drawn stays decoded (BGRA, all frames) in RAM for the process lifetime. Over a long
multi-channel session that grows unbounded (animated emotes dominate: a ~112×112 GIF ≈ 1.5 MB decoded;
a power user can reach 100s of MB–GBs). gpui's bundled `RetainAllImageCache` also never evicts (its
"LRU" doc-comment is a lie — the impl is a plain `HashMap`). So the chat log's images render through a
**custom `LruImageCache`** (`crates/app/src/image_cache.rs`, an `ImageCache` impl, wrapped via gpui's
`image_cache(..)` element around the list — `ChatView::image_cache`). It records each image's last-drawn
time **inside its `load`** — which gpui calls for *every* image it actually renders each frame (both the
layout and paint phases) — so "last accessed" is exactly "last on screen", with **no per-image-kind
bookkeeping in the UI**. (The earlier approach stamped URLs from message data in the render closure; it
was brittle — every image kind it forgot to enumerate, badges/icons/event-emotes, evicted while visible
and vanished.) A timer sweep (`LruImageCache::sweep`, every `EMOTE_SWEEP_INTERVAL`=1min) frees the decoded
frames (and GPU texture via `cx.drop_image`) of anything not drawn within `EMOTE_LIFETIME`=10min
(both match Chatterino's `IMAGE_POOL_*`). Anything on screen was accessed that frame so it's never stale;
an evicted off-screen image re-loads when it scrolls back in (verified: clean, no blank). The cache is
**app-wide** (`LruImageCache::shared`, a gpui global, one sweep timer for the whole app) so an image loaded
in one tab / the picker is reused everywhere.

It's also **disk-backed** like Chatterino: gpui's image loader re-downloads on every cache miss (no disk
cache) and re-decodes, so a short eviction lifetime made emotes that scroll off and back churn the network
and load slowly (a real regression at 60s), and nothing survived a restart. `LruImageCache::load` for a
remote URL writes each fetched image to `<cache>/backseater-cache/images/<hash>` (`bks_auth::store::image_cache_dir`
— NOT `<cache>/backseater`, which on Windows is Velopack's install root and wiped on uninstall/repair)
and on a later miss decodes straight from those bytes — no network. So a first load this session of a
previously-seen emote, and any reload after eviction, is a fast local read; only a truly first-ever
sighting hits the network. Remote-URL decoding is ours (`decode_frames`, mirroring gpui's loader logic +
the **downscale cap** — see the decode bullet in "Key decisions"); embedded/path resources still decode
via gpui's `ImgResourceLoader`. The chat log's animated images render through `animated_img` (not `img()`),
which loads via `image_cache::load_image` — the same slots, so an image is shared between the element and
any `img()` drawing the same URL.

**Downloads use our own client, not gpui's.** gpui's `reqwest_client` pins ALL http to a runtime built
with `worker_threads(1)` ("keep our footprint small") — under a burst (opening the picker = 100s of emotes
at once) that one thread throttled downloads to ~10–50 KB/s (a 4 KB emote took 10–36 s). So image bytes are
fetched by our **own pooled `reqwest::Client` on `bridge::runtime()`** (the multi-thread runtime, see
`image_cache.rs::image_client`/`fetch_bytes`), bridged back to the gpui executor over a oneshot; gpui only
ever decodes from local disk. A `MAX_CONCURRENT_FETCHES`=32 `async_lock::Semaphore` caps in-flight
downloads (so a picker burst drains in waves at full speed, not all-at-once at a crawl). A failed load
becomes a `Slot::Failed(at)` that retries after a 10 s cooldown (never cached permanently — that was a
"blank forever" bug); a corrupt disk file is deleted on decode error so it re-fetches.

**Image size is DPI-aware (Chatterino-style): 1x at 100% scaling, 2x above — never the largest.**
`bks_core::preferred_scale()` holds `1`/`2`, set once at startup from the window's `scale_factor`
(`main.rs`). Emote providers pick the size from it (`seventv.rs` `size_preference`, FFZ `["1","2","4"]`
at 100%, BTTV `1x`/`2x`); **Twitch badges** follow the same rule (`badges.rs` `extend` picks
`image_1x`/`image_2x`). We render emotes at ~26px, so 1x (~32px) is exact at 100% DPI; fetching the `4x`
variant (1–5 MB animated) for a 26px render was a major cause of slow loads + memory. (Kick emotes only
serve `fullsize`; Kick sub badges a single CDN `src`; Kick standard badges are bundled — all unchanged.)
The scale lives in `bks-core` so every crate reads it without a new dependency.

⚠️ Do NOT `clear()` the cache wholesale: clearing
*visible* images leaves them permanently blank, because a virtualized list repaints cached rows without
re-running `request_layout` (so `load()` never re-fires for a row already laid out) — evict only by
last-accessed time. Pure time-based (no hard byte/count cap) for now; a transient spam burst can still
spike but self-clears within a lifetime window. Cold first-load of a big channel's emotes still takes a few
seconds (inherent: many large animated GIFs to download + decode); warm cache is fast.

**Auth / sending / moderation — `Session` is the single source of truth AND observable**
(`app/src/session.rs`). Login is **app-wide**: one `Session` (`Arc`-shared) owns the Twitch + Kick
credentials/actions; login state is mutated *only* through its methods (callable from a tab command
today, a settings screen or a token-refresh task later — tabs never own or assume auth). Every change
fans out two ways so **all tabs stay consistent no matter what caused it** (user logout, token expiry,
failed refresh, login from anywhere):
- a `tokio::sync::watch<LoginState>` snapshot — each tab's `Controller` subscribes once in `start`
  (`watch_login_changes`) and reconciles itself: reconnect its Twitch source when Twitch auth flips
  (authed ↔ anonymous), and reset its send target off Kick when Kick logs out. One cause-agnostic
  loop; later-opened tabs auto-subscribe. **Do NOT add per-command reconnect callbacks** — that was
  the old bug (only the issuing tab reacted); route everything through the broadcast.

Login/logout status (and other status chatter) is **not** shown in chat anymore — it's `tracing::info!`-logged.
`session.rs::broadcast` logs the "logged in/out" line and pushes the new `LoginState` snapshot; there are
no more per-tab notice sinks (`register_tab` is gone). See the notices note below.

Being logged out of a platform is a **normal** state: the tab falls back to an anonymous read
connection (chat still flows; sending/mod disabled) and upgrades live on re-login. The per-tab
`Controller` (`app/src/controller.rs`) owns only this tab's Twitch source, channels, send target, and
seen Kick chatters — it pulls actions/auth from the `Session` at send/connect time. The input box
feeds `controller.handle_input` (routes `/commands` vs chat):
- `/login` `/logout` `/kicklogin` `/kicklogout` → just call `Session` mutators; the subscription does
  the rest. Twitch OAuth is implicit (browser + local server on `localhost:38276`).
- plain text → `TwitchSource::send` (authed IRC) and/or Kick REST, per the tab's send target.
  `/ban`/`/timeout`/`/unban`/`/delete` → `TwitchActions`/`KickActions` pulled from the `Session`.
- **Ban fade + mod notices.** A ban/timeout emits `ChatEvent::ClearChat { platform, user }`, which
  strikes through + fades that user's *past* messages (`ChannelModel::mark_banned`); `/unban` does NOT
  restore them — struck stays struck. A single deleted message (IRC `CLEARMSG`) emits
  `ChatEvent::DeleteMessage` → that one row is struck. The accompanying *notice* text differs by platform:
  - **Twitch, not a mod** (IRC `CLEARCHAT`): carries only target + timeout duration — **not** the
    moderator, and IRC sends **no unban event** at all. So `chatview.rs` emits a generic
    "X was timed out / banned" notice.
  - **Twitch, moderator/broadcaster** (EventSub — see the moderator-feed note below): rich notices
    with moderator + duration + reason arrive as `ChatEvent::Notice`; `ChatEvent::ModFeed` tells the
    UI to suppress its generic fallback while the feed is live (the fade still comes from IRC).
  - **Kick** (Pusher `UserBannedEvent`/`UserUnbannedEvent`): the connector has the moderator +
    duration, so it posts the rich notice itself (`ban_notice`/`unban_notice` in `kick/connector.rs`,
    e.g. "mod timed out user for 1h30m", "mod unbanned user"); `chatview.rs` then only fades for Kick
    (no generic notice) to avoid doubling. Unban is notice-only (no un-fade).

**Twitch moderator feed (EventSub, `crates/twitch/src/eventsub.rs` + `eventsub_manager.rs`).** When
logged in AND a moderator/broadcaster of the tab's Twitch channel, the feed subscribes (Helix `POST
/eventsub/subscriptions`, websocket transport, user token, cost 0) to:
- **`channel.moderate` v2** → every mod action *with the acting moderator* formatted into a
  `ChatEvent::Notice` ("mod timed out user for 10m: reason", unban/untimeout, delete + clipped body,
  clear, warn, slow/emote/followers/sub-only toggles, raids, mod/VIP grants, blocked/permitted terms,
  unban requests; `shared_chat_*` variants read like the plain ones). Needs the big read-scope set
  (see `SCOPES` in `auth/src/twitch.rs`) — **a token from before those scopes leaves the feed off
  until the next `/login`** (logged as a hint, chat still works).
- **`automod.message.hold` / `.update` v2** (scope `moderator:manage:automod`) → `ChatEvent::
  AutoModHeld` renders an amber row (chatter, held text, reason, **Allow/Deny** chips →
  `Helix::manage_automod_message`); the `.update` resolves the row in place ("✔ allowed by mod").
⚠️ **One shared socket app-wide, NOT one per tab.** Twitch caps EventSub at **3 WebSocket
connections with enabled subscriptions per (client id, user id)**; the old per-tab socket 429'd
("number of websocket transports limit exceeded") the moment a user modded 4+ channels (or a startup
burst raced for slots), and the per-tab reconnect loop then retried forever, each retry opening yet
another socket. So `eventsub_manager.rs` owns a **single** socket for the logged-in user and
multiplexes every tab onto it: tabs `register_eventsub(auth, broadcaster_id, sink)` → get back an
`EventsubRegistration` guard; the manager creates that channel's subscriptions on the shared session
and routes each notification to the owning sink by `subscription.condition.broadcaster_user_id`. One
socket holds 300 subscriptions (~100 channels at 3 subs each). A background `socket_task` runs the
connection + reconnect-with-backoff, re-subscribing all registered channels on each fresh session; an
internal command channel handles register/unregister without locks across awaits. A **new login**
(different `client_id`/`user_id`/token) rebinds the socket (old task retires + deletes its subs). The
transport-limit 429 is now recognized (`is_transport_limit`) and backed off 60s instead of hammering.
`eventsub.rs` keeps the notice/automod formatting + the `subscribe`/`delete_subscription` helpers;
`run()` is gone.

Wiring: `session.rs` carries an `EventsubAuth` (token + scopes) → `controller.connect_twitch` passes
it to `bridge::run_twitch` → `spawn_eventsub` registers on the first `Channel` meta (needs the room
id) next to the Hermes points feed, storing the `EventsubRegistration` in the tab's `TaskGuard`.
The guard **aborts the points feed AND drops the registration when the IRC connection ends** (login
swap/channel change) so a re-join can't stack duplicates and the channel's subscription slots free up.
A subscription 403 = not a mod there — normal, that channel's feed stays off, no retry.

**Why two async worlds:** `tmi`/networking need tokio; GPUI has its own (smol-based) executor.
We run ONE multi-threaded tokio runtime on its own threads (`bridge.rs`, kept alive via
`OnceLock`) and bridge to GPUI with a smol channel. The tokio mpsc type never crosses into the UI.

## Key decisions & rationale

- **`tmi`** for Twitch chat read (fastest IRC parser; same crate can send later). Chosen over `twitch-irc`.
- **Send + moderation libraries are intentionally undecided.** Twitch removed `/ban` etc. from IRC
  (Feb 2023), so actions MUST go through the Helix REST API — but which crate (or hand-rolled
  `reqwest`) is a later decision. Traits are defined now so wiring won't churn.
- **Rendering = "Route A"** (flex-wrap of word/emote tokens). GPUI has *no* native inline-box-in-text
  primitive (the feature was closed "not planned" in the Zed repo). A custom glyph-level `Element`
  ("Route B") is the future upgrade; keep it isolated to `crates/app/src/render.rs` so only that file
  changes.
- **gpui-component kit** chosen over raw gpui for ready-made widgets.
- **Animated images render through our own `AnimatedImage` element, NOT gpui's `img()`**
  (`crates/app/src/animated_img.rs`, constructor `animated_img(id, url, height)`). gpui's `img()`
  freezes GIF/WebP animation while the window is unfocused and drives frames with
  `request_animation_frame()` every layout pass — pinning the whole window to the display refresh
  rate (60–144fps) for a ~10fps emote, which made the OS window-move loop stutter (gpui has no
  partial-rect repaint, so the repaint *rate* is the only lever). `animated_img` fixes both: it
  paints one frame of the cached `RenderImage` via
  `Window::paint_image(.., frame_index, ..)` and schedules its own repaints at the animation's real
  cadence, **quantized to a shared ~20ms grid** (`next_tick` + a process epoch, `ANIM_TICK`) and
  **coalesced to one timer per view per tick** (`schedule_wakeup` + a pending-wakeup map keyed by
  `EntityId`) — Chatterino's single global `GIFTimer` (20ms). The coalescing matters: the first
  version detached one timer task per element per layout pass, so N visible animated emotes fired
  N notify tasks every 20ms (thousands of main-thread wakeups/sec) and window drags stuttered
  again once a chat was emote-heavy. The element
  needs a **stable id** (it keys the per-element frame state); width follows the image's aspect,
  `.max_w()` gives object-fit-contain for the picker's fixed cells. Interactivity (click/tooltip)
  stays on wrapper divs, as with `img()`. ⚠️ Never render a *possibly-animated* image (emotes,
  third-party badges) with plain `img()` — with an `.id()` it animates focus-gated at vsync cost,
  without one it freezes on frame 0. Static images (platform icons, bundled Kick badges, posters,
  avatars) stay `img()`.
- **Oversized images are downscaled at decode time** (`image_cache.rs::decode_frames` →
  `finish_frame`, cap `max_decode_height()` = 80px × display scale). Decoded BGRA frames cost
  `w×h×4×frames` in heap **and** GPU atlas for as long as they're resident — a 500×500 46-frame
  Kick GIF is 44 MB decoded for a ≤64px render; capped it's <1 MB. (Freeing decoded frames after
  GPU upload, Chatterino-style, is NOT expressible in gpui's public API — the atlas is keyed by
  `RenderImage.id`, whose frames only live behind the same `Arc` you must keep to paint — so the
  decode-time cap is the memory lever.) Measured on a real cache (4,248 images): resident decoded
  frames cost a few tens of MB in heavy sessions — the cap removes the pathological tail (Kick
  `fullsize`-only CDN, legacy 4x files). Remote-URL decode is ours (`decode_frames`, mirrors gpui's:
  GIF/animated-WEBP frame iteration, skip individually corrupt frames, RGBA→BGRA); embedded/path
  resources still go through gpui's `ImgResourceLoader`.
- **Format: WEBP for everything (animated + static).** `SeventvProvider::best_image_url` prefers
  WEBP (smaller than GIF — no 256-color palette, better compression), GIF only as a fallback. Our
  decoder handles animated WEBP (`WebPDecoder::has_animation` → `into_frames`) the same as GIF.
  AVIF is skipped — the `image` crate can't decode it here.
- **The tab-chip live-status tooltip is hand-rolled** (`main.rs`: `chip_tip*` fields,
  `chip_hover_changed`/`schedule_chip_tip_hide`/`dismiss_chip_tip`, `chip_tooltip` +
  `live_tooltip_content`), not gpui's `hoverable_tooltip` — gpui's can't be dismissed
  programmatically, so it sat on top of the chip's right-click context menu, and its 500ms hide
  grace dragged stale tooltips between adjacent chips. The overlay is
  an absolute div under the chip (chip is `relative()`), `deferred()` so the log doesn't paint over
  it; hovering the panel keeps it up (clickable channel links), any chip click/drag dismisses.
  Show delay 300ms, hide grace 250ms (`CHIP_TIP_SHOW_DELAY`/`CHIP_TIP_HIDE_GRACE`).

## ⚠️ GPUI dependency pinning (critical, read before touching `app/Cargo.toml`)

GPUI is **unpublished**. `crates/app/Cargo.toml` pins these to a **specific `zed-industries/zed`
git rev** that must match the rev `gpui-component` is built against, or the build breaks:

- `gpui`, `gpui_platform`, `reqwest_client` → `zed` rev `1d217ee39d381ac101b7cf49d3d22451ac1093fe`
- `gpui-component`, `gpui-component-assets` → gpui-component rev `063e55bbc4fb13907a988111e3581595cbcaefde`

To upgrade: pick a new gpui-component rev, read the gpui rev from *its* `Cargo.toml`, and bump
both together. `reqwest_client` is needed so `img(<https url>)` can fetch remote emote images;
it's registered in `main.rs` via `cx.set_http_client(...)`. When bumping the rev, also re-check
the gpui surfaces `animated_img`/`image_cache` build on:
`Window::paint_image`/`with_optional_element_state`/`current_view`, the `Element` + `ImageCache`
traits, and `RenderImage::new/size/delay/frame_count`.

## Build & run

```sh
cargo build                 # whole workspace
cargo test                  # unit tests (all crates)
cargo run -p backseater     # run the app
```

Channels are set per tab (right-click a tab → Settings); tabs persist to
`<config>/backseater/tabs.json`. `BKS_DEBUG=1` env var logs received messages to stderr (handy
when you can't see the window).

**Releasing:** bump the workspace `version` in `Cargo.toml` (+ a CHANGELOG section) and push —
CI auto-publishes the installer + update feed to GitHub Releases (`docs/RELEASING.md`).
Never push `v*` tags by hand; the publish step creates them.

**Twitch login (send/moderate):** type `/login`; `/logout` clears it. The app ships with a built-in
Twitch Client ID (`DEFAULT_CLIENT_ID` in `crates/auth/src/twitch.rs`, redirect `http://localhost:38276`)
— a Client ID is **not a secret**, so it's safe to embed (only the Client *Secret* is, and the
implicit flow uses none). Override with `BKS_TWITCH_CLIENT_ID`. Token saved to the **OS keyring**
(Windows Credential Manager, `store.rs::save_secret`; a plaintext `twitch_credentials.json` is
the fallback if the keyring errors, cleaned up on the next successful save). Flow: implicit
(`response_type=token`) against
`id.twitch.tv/oauth2/authorize`, validated at `/oauth2/validate`. Moderation uses Helix
(`/helix/moderation/bans`, `/moderation/chat`).

**Kick login (send/moderate):** type `/kicklogin`; `/kicklogout` clears it. Kick **requires a client
secret** for its authorization-code + PKCE flow, which must not ship in the binary — so a small
**Cloudflare Worker broker** (`worker/`) holds the secret and does the token exchange/refresh on the
app's behalf. The desktop app does the browser login locally (PKCE proves it's legit), then calls the
broker for the secret step. The broker URL is baked in (`DEFAULT_BROKER_URL` in
`crates/auth/src/kick.rs`; not a secret — override with `BKS_KICK_BROKER_URL`), so `/kicklogin`
works out of the box. Tokens (access + refresh + the broker URL) saved to the **OS keyring** like
Twitch's (same file fallback, `kick_credentials.json`).
The Kick app id/secret live ONLY as Worker secrets — see `worker/README.md`.
Send = `POST /public/v1/chat`; ban/timeout = `POST /public/v1/moderation/bans` (duration in
**minutes**; `/timeout` seconds are converted). Kick's API can't resolve a username → id, so
`/ban`/`/timeout` only work on chatters we've **already seen** send a message (the controller
remembers `login → id`). No delete endpoint.

The broker (`worker/src/index.js`) is **OAuth-only**, intentionally minimal + locked down: POST-only
token/refresh endpoints + a public `GET /kick/config` (client id), not a general proxy; `redirect_uri`
pinned server-side; secrets via `wrangler secret put`; nothing logged. The anonymous Kick *reads*
(channel/emotes/usercard/history) do **not** go through it — they run in-process via `wreq` (see the
Kick note below).

**Send target:** when logged into both platforms, a toggle by the input box cycles Twitch → Kick →
Both. `Both` sends typed messages to both chats; mod commands are disabled in `Both`.

## Platform notes

### Windows (primary development target)
- Install Rust via `rustup` with the **MSVC toolchain** (needs Visual Studio Build Tools, C++ workload).
- GPUI uses the **DirectX** backend — GPU-accelerated, matches what users get.
- **`wreq` build prerequisites (NASM + LLVM/libclang).** `bks-kick` depends on `wreq`, which compiles
  BoringSSL from source. On Windows that needs two extra tools beyond the C++ workload — they are NOT
  optional, the build hard-fails without them:
  - **NASM** (`winget install NASM.NASM`) — BoringSSL's x64 assembly. Without it: *"No
    CMAKE_ASM_NASM_COMPILER could be found"*.
  - **LLVM** (`winget install LLVM.LLVM`) — `bindgen` needs `libclang.dll`. Set `LIBCLANG_PATH` to its
    `bin` dir (e.g. `C:\Program Files\LLVM\bin`). Without it: *"Unable to find libclang"*.
  Both must be on `PATH` (and `LIBCLANG_PATH` set) for `cargo build`; easiest is to add them to the
  user PATH permanently so a plain `cargo run` works. The MSVC C++ compiler must also be reachable —
  run from a *Developer* prompt or after `vcvars64.bat`. (A *very long* build path can also trip
  BoringSSL's CMake probe — keep the repo at a short path if you hit *"Cannot open compiler generated
  file"*.)
- Just `cargo run -p backseater`.

### Linux / macOS (planned for later)
- Not currently tested or supported — will need `x11`/`wayland` features re-enabled in
  `gpui_platform` and platform-specific build setup when the time comes.
- The non-GUI crates (`core`/`platform`/`twitch`/`emotes`) build and test anywhere.

## Protocol reference notes

Hard-won protocol details and JSON event shapes, kept here so they don't have to be
re-discovered:
- Kick chat = **Pusher WebSocket** (implemented in `crates/kick`):
  - WS: `wss://ws-us2.pusher.com/app/32cbd69e4b950bf97679?protocol=7&client=js&version=8.4.0&flash=false`.
  - Subscribe: `{"event":"pusher:subscribe","data":{"auth":"","channel":"chatrooms.{chatroom_id}.v2"}}`.
  - Chat frames: event `App\Events\ChatMessageEvent`; the Pusher `data` field is a JSON *string*
    (parse twice) → `{id, content, created_at, sender:{username, identity:{color}}}`.
  - Inline emotes: `[emote:{id}:{name}]` in `content`; CDN `https://files.kick.com/emotes/{id}/fullsize`.
  - Respond to `pusher:ping` with `{"event":"pusher:pong","data":{}}` to stay connected.
  - ⚠️ **Cloudflare fronts the read endpoints** (`kick.com/api/v2/channels/{slug}`, `kick.com/emotes/
    {slug}`, the per-channel users endpoint, and `web.kick.com/.../history`): it fingerprints the TLS
    ClientHello and 403s *every plain* in-process Rust client (rustls AND native-tls return "Request
    blocked by security policy"); only curl/browser/edge fingerprints pass. We pass it **in-process**
    with **`wreq`** (a reqwest-shaped client that forges a real Chrome BoringSSL handshake) — a shared
    process-wide `Client` with `Emulation::Chrome*` + browser-looking headers (`crates/kick/src/api.rs`,
    `kick_get`). These were previously proxied through the broker Worker's edge `fetch`; that's gone —
    the **broker is OAuth-only** now. ⚠️ `wreq` builds BoringSSL from source, so on Windows it needs
    **NASM + LLVM/libclang** at build time (see the build-prerequisites note). The Pusher WS itself is
    NOT behind Cloudflare and works with plain rustls — but the dep tree has both `ring` and `aws-lc-rs`,
    so the connector calls `ring::install_default()` once or TLS panics.
  - **Channel lookup**: `fetch_channel_info(channel)` → `{chatroom_id, user_id, channel_id,
    subscriber_badges, is_live, …}` (one call per channel join, not per message). Yields per-tier
    **subscriber badge** images, matched to each chatter's month count in `builder.rs`. When offline it
    also fetches the latest VOD (`/channels/{slug}/videos`) for the offline tooltip.
  - **Usercard** (`channels/{channel}/users/{slug}`): returns this chatter's standing *in the channel* —
    `following_since`, `subscribed_for` (months), `is_moderator`, avatar. Richer than the account-level
    channel lookup; `fetch_user_info(channel, slug)` → `KickUserInfo`, shown like the Twitch card.
  - **Chat history** (`web.kick.com/api/v1/chat/{id}/history`): the join backlog. ⚠️ The history endpoint
    keys on the v2 `chatroom.channel_id` (== top-level channel `id`), **not** the Pusher `chatroom.id`
    (passing the Pusher id returns an empty `messages: []`). `fetch_channel_info` returns `channel_id`,
    so the connector passes it straight to history (a `slug`→id lookup is a fallback when it's 0).
    `crates/kick/src/history.rs::fetch_recent` reverses the (newest-first) list, parses each (same shape
    as a live `ChatMessageEvent`, except `metadata` is a JSON *string* there) through the live
    `build_message`, flagged `historical`. Fetched in a **spawned task** (doesn't block the live
    read loop); the UI sorts `historical` messages ahead of live ones by timestamp, so Twitch + Kick
    history interleave chronologically regardless of arrival order (`ChannelModel::insert_message`).
- Twitch native emote URL: `https://static-cdn.jtvnw.net/emoticons/v2/<id>/default/dark/2.0`.
- Twitch **badges** (no auth, `crates/twitch/src/badges.rs`): POST `https://gql.twitch.tv/gql` with
  the public web `Client-Id: kimne78kx3ncx6brgo4mv6wki5h1ko` and the `ChatList_Badges` persisted
  query (`variables.channelLogin`). ⚠️ The response splits badges into **two arrays** —
  `data.badges` (globals: staff/turbo/generic subscriber 0–6/...) AND
  `data.user.broadcastBadges` (the channel's own subscriber tiers + VIP). You **must merge both**
  or channel-specific subscriber tiers (e.g. `subscriber/3120`) are missing and sub badges vanish.
  The IRC `badges` tag keys badges as `set-id/version`; map that to the merged map's image URL.
- Kick **badges**: sent inline in the chat event (`identity.badges[]`, each `{type, count?}`).
  Standard types (mod/vip/og/founder/staff/broadcaster/verified/sub_gifter/bot/sidekick/
  trainwreckstv) have NO public CDN — we bundle them
  (`crates/app/assets/kick/badges/*.webp`,
  served by `app/src/assets.rs`'s `AssetSource`). `img("kick/badges/<type>.webp")` resolves to the
  embedded bytes. **Subscriber** badges instead carry a per-tier CDN image from the channel lookup
  (`subscriber_badges[]`, `{months, src}`); the connector matches each chatter's `count` to the
  highest tier they meet and fills `Badge.url` directly (so the bridge keeps badges that already
  have a url and only resolves the empty standard ones).
- YouTube live chat = **InnerTube** (YouTube's private web API), anonymous, no key/OAuth/quota
  (implemented in `crates/youtube`, read-only). Endpoints (all `POST`, `?key=<INNERTUBE_API_KEY>`):
  - **Bootstrap** the session by scraping `https://www.youtube.com/embed/jNQXAC9IVRw` HTML for
    `INNERTUBE_API_KEY` / `INNERTUBE_CLIENT_VERSION` / `visitorData` (`api.rs`). Every request wraps its
    body in `{context:{client:{clientName:"WEB", clientVersion, hl, gl}}}` and sends
    `X-Goog-Visitor-Id: <visitorData>` + a `watch?v=` referer. A `CONSENT`/`SOCS` cookie skips the consent
    wall. Not Cloudflare-gated (unlike Kick) — plain rustls `reqwest` passes, no `wreq`.
  - **Source → live videoId** (`resolve.rs`): direct video refs (bare 11-char id, `watch?v=`, `youtu.be/`,
    `/live/`, `/shorts/`, `/embed/`) resolve instantly; an `@handle` / `/channel/UC…` / `/c/` / `/user/`
    resolves the `UC…` id (scrape the channel page), then probes the *current* live video via
    `GET /embed/live_stream?channel=<UC…>` (regex the canonical `watch?v=`). `None` when offline → the
    connector waits + retries (offline is normal, no error row).
  - **`youtubei/v1/next`** `{videoId}` → initial live-chat continuation at
    `contents…conversationBar.liveChatRenderer.continuations[]`, plus live title / owner `UC…` id / start.
  - **`youtubei/v1/live_chat/get_live_chat`** `{continuation}`, long-polled: walk
    `continuationContents.liveChatContinuation.actions[].addChatItemAction.item`; each item is a
    single-keyed `{<rendererName>: {…}}`. Next token + delay from
    `continuations[].timedContinuationData`/`invalidationContinuationData` (`continuation` + `timeoutMs`,
    floored to 1s). Empty/expired continuation → re-`next`; offline → `Live{live:false}` + wait.
  - **Renderers** (`builder.rs`): `liveChatTextMessageRenderer` → `Message` (author name/color/`UC…` id,
    `message.runs[]` text + `emoji` runs; `isCustomEmoji:true` emoji → inline `Emote` from the best
    `image.thumbnails[]`, unicode emoji fall back to `shortcuts[0]` text; `authorBadges[].liveChat`
    `AuthorBadgeRenderer.customThumbnail` → member `Badge` + green name). Super Chat
    (`liveChatPaidMessageRenderer`) / Super Sticker → `Event{Bits}`; `liveChatMembershipItemRenderer` →
    `Event{Sub}`; `liveChatSponsorshipsGiftPurchaseAnnouncementRenderer` → `Event{Gift}`. `timestampUsec`
    (µs epoch) → timestamp. There is **no push socket** — the browser polls, so we do too; the initial
    page's backlog is skipped (recorded as seen) so join doesn't dump a wall of old messages.
- 7TV API (used by `SeventvProvider`): global `GET https://7tv.io/v3/emote-sets/global`, channel
  `GET https://7tv.io/v3/users/twitch/<room-id>` (→ `emote_set.emotes`); CDN `https:<host.url>/<file>`.
  YouTube uses `/users/youtube/<UC…>` (`SeventvProvider::for_youtube()`), Kick
  `/users/kick/<id>` (`for_kick()`).
  **Caching:** `SeventvProvider` shares one `reqwest::Client` and a process-wide URL-keyed cache
  (10-min TTL) across all instances, so the (identical) global set isn't re-fetched per connection and
  a channel's set survives the reconnect storm a login flip causes (every tab re-joins). 404s cache an
  empty set so they aren't retried. This was the fix for the repeated "loaded N global/channel 7TV
  emotes" log spam.

## Conventions (user's global prefs)

- **Commit messages: one sentence.**
- **Comment only when code is genuinely complex** — the existing files follow this; match it.
- **The user runs and validates the app, not Claude.** This is a live GUI chat client — Claude
  cannot meaningfully see whether emotes/badges/icons render. After building, **stop and let the
  user open the program**; they validate visually and via the `BKS_DEBUG=1` stderr log themselves.
  Claude's job ends at: it compiles cleanly, tests + clippy pass, and the logic is sound. Do **not**
  spawn `backseater.exe` to "verify" UI — confirm the build, then hand off.
