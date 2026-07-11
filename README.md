# Backseater

A multi-platform live-chat desktop client in Rust + [GPUI](https://www.gpui.rs/)
(the UI framework behind the Zed editor) with
[gpui-component](https://github.com/longbridge/gpui-component).

One window, many tabs: each tab merges **Twitch**, **Kick**, and **YouTube** live
chat into a single feed, with third-party emotes, badges, moderation, and the
creature comforts of a mature chat client.

## Features

- **Tabs** — each tab is an independent channel set (Twitch and/or Kick and/or
  YouTube), with its own merged feed and send target. Tabs persist and restore;
  a tab can be popped out into its own OS window (a live mirror sharing the same
  buffer and connection).
- **Twitch** — chat over IRC (one shared read + one shared write connection for
  the whole app), native emotes, sub/VIP/mod badges, recent-message backlog on
  join, send + `/ban` `/timeout` `/unban` `/delete` via Helix, an EventSub
  moderator feed (rich mod notices + AutoMod allow/deny), pinned messages,
  viewer list (mod-only, per Twitch's API rules), and usercards with follow/sub
  age.
- **Kick** — chat over its Pusher WebSocket, inline + subscriber badges, public
  events (subs, gifts, hosts, kicks, reward redemptions), history backlog,
  pinned messages, send + ban/timeout via the public API, usercards.
- **YouTube** — anonymous live-chat reading via InnerTube (no API key, no
  quota): messages, custom channel emojis, membership badges, Super Chats /
  memberships / gifts as event rows.
- **Emotes** — 7TV, BTTV, and FFZ (global + per-channel), animated emotes play
  at their real cadence (including while the window is unfocused), an emote
  picker with search, tab-completion for emote and chatter names, and 7TV
  cosmetics (name paints + badges).
- **Chat niceties** — colored names with contrast correction, mentions + links,
  reply threads, first-message highlight, mentions panel (per-tab or all-tabs),
  ignore list, per-user highlights, event feed panel, clean ban/timeout/delete
  strikes.
- **App** — dark/light/custom themes, any installed font, streamer mode
  (auto-detects OBS etc.), settings and usercards as real child OS windows, a
  disk-backed self-evicting image cache, a virtualized log that stays smooth in
  fast chats, and OAuth tokens stored in the OS keyring (Windows Credential
  Manager) rather than plaintext files.

## Credits

Backseater is heavily inspired by **[Chatterino2](https://github.com/Chatterino/chatterino2)**
and **[Mergerino](https://github.com/Fixlation/Mergerino)** (a Chatterino fork with
Kick/YouTube support). It is not a
port — no code is shared — but a lot of the functionality, protocol knowledge,
and design ideas come from studying them: the shared IRC connection model, the
channel/view split, third-party emote handling, image-cache eviction and sizing
strategy, the global animation tick, streamer mode, the recent-messages backlog,
and many smaller behaviors. Thanks to those projects and their contributors.

## Architecture

```
crates/
  core/      # platform-agnostic domain model (Message, MessageElement, ...)
  platform/  # the expandability seam: ChatSource trait + ChatEvent
  twitch/    # Twitch: shared IRC, Helix, EventSub, badges, history
  kick/      # Kick: Pusher WebSocket, REST, badges, history
  youtube/   # YouTube: anonymous InnerTube live-chat reader
  emotes/    # EmoteRegistry + EmoteProvider trait (7TV / BTTV / FFZ)
  auth/      # OAuth flows + JSON persistence
  app/       # GPUI binary: bridge, rendering, tabs, settings, windows
worker/      # Cloudflare Worker holding the Kick OAuth client secret
```

The design goal: **adding a platform = implement one trait + one message
builder, with zero UI changes.** Connectors emit platform-tagged `ChatEvent`s
over a channel; the UI renders `Message` tokens without knowing where they came
from. Networking runs on one tokio runtime, bridged to GPUI's own executor.

## Build & run

Windows is the primary target (GPUI's DirectX backend). Requires:

- Rust (MSVC toolchain) + Visual Studio Build Tools with the C++ workload
- **NASM** (`winget install NASM.NASM`) and **LLVM** (`winget install LLVM.LLVM`,
  with `LIBCLANG_PATH` set to its `bin` dir) — the Kick crate's `wreq` client
  builds BoringSSL from source

```sh
cargo build                 # whole workspace
cargo test                  # unit tests
cargo run -p backseater     # run the app
```

Channels are set per tab (right-click a tab → Settings). `/login` starts the
Twitch OAuth flow, `/kicklogin` the Kick one. `BKS_DEBUG=1` logs received
messages to stderr.

## GPUI dependency pinning

GPUI is unpublished, so `crates/app/Cargo.toml` pins `gpui`/`gpui_platform`/
`reqwest_client` to the exact `zed-industries/zed` revision that the chosen
`gpui-component` revision is built against. Bump both together.

## License

[MIT](LICENSE)
