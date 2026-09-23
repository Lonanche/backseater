# Twitch chat feature review

Reviewed: 23 September 2026. Scope: Backseater's implementation compared with
Twitch's documented native chat experience. This is a source review, not a live
GUI test. Feature availability can depend on channel settings, role, subscription
and Twitch rollout.

Backseater already covers everyday Twitch chat and routine moderation well. The
biggest gaps are interactive viewer features, Shared Chat context and the
remaining Mod View tools.

## Already implemented

- Native emotes, badges, GIF receiving, emote picker, autocomplete, replies and
  threads, mentions and first-message highlights.
- Pins, announcements, subscription/gift/raid notices, Channel Points redemption
  notices, highlighted messages and chat-mode indicators.
- Ban, timeout, unban, delete, warnings, AutoMod allow/deny, suspicious-user
  controls, mod/VIP management, raids and shoutouts.
- Additional conveniences: merged platforms, third-party emotes, search,
  configurable highlights, themes and popouts.

## Priority 1: rejected-message feedback

At review time, Twitch's rejected-message feedback was discarded. The IRC write
handler handled ping/reconnect but ignored `NOTICE`, while the composer cleared
after Enter. A rejection for slow mode, subscriber restrictions, verification or
timeout could therefore have no visible explanation. Native Twitch shows why a
message failed. See [Twitch chat basics](https://help.twitch.tv/s/article/chat-basics)
and the [NOTICE reference](https://dev.twitch.tv/docs/chat/irc/#notice-reference).

Relevant implementation:

- [IRC handlers](../crates/twitch/src/irc_manager.rs): `handle_read`, `handle_write`.
- [Composer](../crates/app/src/chatview.rs): `on_input_event`, `record_sent`,
  `history_recall`.

The fix accompanying this review routes rejection and held-message notices from
both IRC connections into the affected channel's visible error rows, preserving
Twitch's reason. Connection-wide notices reach all registered channels. Routine
mode/status notices remain suppressed because their state is already handled
elsewhere. Existing input history allows Up to recall the failed text and Down to
restore a newer draft; messages are not automatically resent.

Only priority 1 is being implemented. The remaining gaps below are recorded for
future work.

Validation for priority 1: all 370 workspace tests pass, including rejection
routing and a headless GUI test covering failed-message recall without losing a
new draft. Workspace Clippy with warnings denied and the debug app build pass.
Live Twitch/desktop visual validation was not performed.

## Feature gaps

| Feature | What Backseater has | Missing compared with native Twitch |
| --- | --- | --- |
| Shared Chat | Incoming messages and some shared moderation notices | Session indicator, participating channels, message origin and source-channel badges. The message conversion does not preserve Twitch's source metadata. [Native behavior](https://help.twitch.tv/s/article/shared-chat?language=en_US) |
| Channel Points | Redemption notices and highlighted messages | Your balance, bonus claiming, reward browsing and actually redeeming rewards. Receiving a redemption is supported; participating is not. [Twitch guide](https://help.twitch.tv/s/article/viewer-channel-point-guide?language=en_US) |
| Polls and predictions | No dedicated integration found | Active cards, timers, results, voting/predicting and creator management. [Twitch APIs](https://dev.twitch.tv/docs/api/reference#get-polls) |
| Bits and Cheermotes | Bits-badge milestone notices | Actual Cheer amounts and animated Cheermote rendering, plus the Bits balance/purchase/send experience. The parser handles emotes/GIFs but does not interpret Cheer metadata. [Native cheering](https://help.twitch.tv/s/article/guide-to-cheering-with-bits) |
| Power-Ups and Hype Trains | Some underlying sub/gift activity appears | Dedicated effects/reward interactions and Hype Train progress, levels and countdowns. [Power-Ups](https://help.twitch.tv/s/article/power-ups?language=en_US), [Hype Trains](https://help.twitch.tv/s/article/hype-train-guide) |
| Whispers | No implementation found | Send/receive private messages, conversation list and unread notifications. [Whisper APIs](https://dev.twitch.tv/docs/chat/whispers/) |
| Advanced moderation | Individual-message actions and AutoMod decisions | Shield Mode, pending unban-request queue, approve/deny appeals, blocked/permitted-term editing and AutoMod settings. Logging another moderator's action does not provide its management UI. [Twitch moderation tools](https://dev.twitch.tv/docs/chat/moderation) |
| Full moderation history | Buffered chat and live moderation notices | Twitch-backed user history and shared moderator comments. The existing Mod card button opens Twitch for this. [Native Mod View](https://help.twitch.tv/s/article/mod-view) |
| GIF sending | GIF parsing and rendering | GIF search/picker, eligibility handling and sending. Native Twitch exposes a GIF tab for eligible subscribers. [Chat basics](https://help.twitch.tv/s/article/chat-basics) |
| Chat identity | Displays names, colors and badges | Change your Twitch username color and select global/per-channel badges. [Color API](https://dev.twitch.tv/docs/api/reference#update-user-chat-color), [badge selection](https://help.twitch.tv/s/article/how-to-use-badges) |
| Subscriptions and gifts | Displays their events and associated messages | In-app subscription/gift purchasing and renewal-sharing flows. [Native subscriptions](https://help.twitch.tv/s/article/support-subscriptions?language=en_US) |

Smaller gaps include Twitch account block-list synchronization (the ignore list
is local), and dedicated chat-rule, verification and warning-acknowledgement
flows.

## Implementation evidence

- [Message conversion](../crates/twitch/src/connector.rs):
  `privmsg_to_message` preserves replies, first-message and reward information,
  but not Shared Chat origin or Cheer metadata.
- [Message elements](../crates/twitch/src/builder.rs):
  `build_privmsg_elements_with_gifs` handles emote and GIF ranges.
- [Hermes subscriptions](../crates/twitch/src/pubsub.rs): points redemptions,
  pins and viewer counts; no balance, claim or redemption actions.
- [EventSub routing](../crates/twitch/src/eventsub.rs): moderation, AutoMod and
  suspicious-user events; no poll, prediction or Hype Train integration.
- [Twitch actions](../crates/twitch/src/actions.rs),
  [Helix client](../crates/twitch/src/helix.rs),
  [command registry](../crates/app/src/commands.rs) and
  [OAuth scope tiers](../crates/auth/src/twitch.rs): the existing action surface
  does not implement the missing viewer/moderator workflows.
- [Usercard](../crates/app/src/usercard.rs): the Mod card button delegates full
  Twitch moderation history to the browser.
- [Local ignore rules](../crates/core/src/ignore.rs): filtering within Backseater,
  rather than synchronization with Twitch's account block list.

## Feasibility and suggested order

Shield Mode, unban requests, whispers and username color have documented APIs.
Poll, prediction and reward management require broadcaster authorization; those
management endpoints do not provide ordinary viewer voting or spending. A Twitch
browser handoff is a practical initial solution for those interactions. See the
[API reference](https://dev.twitch.tv/docs/api/reference).

1. Fix rejected-send feedback, with clear reasons and easy retry. Addressed by
   the accompanying change; retry uses the existing input history.
2. Add Shared Chat context and Cheermote rendering so messages retain their
   native meaning.
3. Finish moderator tools: Shield Mode, unban queue and term management.
4. Add whispers and chat identity controls.
5. Add participation cards for points, polls, predictions and Hype Trains,
   using browser handoffs where necessary.
