# Kick Pusher events — what we handle and what we ignore

Reference captured from a full live session (2026-07-07). Every event below arrives on the
Pusher WebSocket as `App\Events\<Name>` (prefixed) or bare `<Name>`; the connector strips the
`App\Events\` prefix and matches on the bare name (`crates/kick/src/connector.rs`). Run with
`RUST_LOG=bks_kick=debug` to log every event (name + raw payload); plain `ChatMessageEvent`s are
excluded from that log to avoid flooding it.

This doc exists so that if we later want to act on an event we currently ignore, we already have
its real payload shape and know why it was skipped.

## Handled (produce a `ChatEvent`)

| Event | What we do | Payload notes |
|---|---|---|
| `ChatMessageEvent` | `Message` | Normal chat line. `content`, `sender.identity.{color,badges,badges_v2}`. |
| `SubscriptionEvent` | `Event{Sub}` | `{chatroom_id, username, months}`. **No message field** — see resub note below. |
| `GiftedSubscriptionsEvent` | `Event{Gift}` | `{gifter_username, gifted_usernames[], gifter_total}`. `gifter_username:"Anonymous"` for anon gifts. |
| `StreamHostEvent` | `Event{Raid}` | Kick's host (its "raid" equivalent). `{host_username, number_viewers}`. |
| `KicksGifted` | `Event{Bits}` | Kick's bits/cheer. `{sender, gift:{name, amount, ...}}`. |
| `RewardRedeemedEvent` | `Event{Reward}` | Channel-point redemption. `{reward_title, username, user_input}`. |
| `UserBannedEvent` | `ClearChat` + `Notice` | `{user, banned_by, permanent, duration?, expires_at?}`. `banned_by.id:0` when the actor id is hidden but name is present. |
| `UserUnbannedEvent` | `Notice` | `{user, unbanned_by, permanent}`. No un-fade. |
| `MessageDeletedEvent` | `DeleteMessage` (+ `Notice`) | `{message.id, aiModerated, violatedRules[]}`. AI moderation deletions dominate (`is_spam-TRUE`, `hate`, `self_harm`, ...). |
| `PinnedMessageCreatedEvent` | `PinMessage` | `{message (full chat shape), duration (seconds as string), pinnedBy}`. |
| `PinnedMessageDeletedEvent` | `UnpinMessage` | Payload is `[]`. |
| `StreamerIsLive` | `Live{live:true}` | `{livestream:{session_title, created_at}}`. |
| `StopStreamBroadcast` | `Live{live:false}` | `{livestream:{id, channel:{id}}}`. |
| `ChatMessageSentEvent` | (buffered) | Carries a resub's `optional_message`; buffered by username and attached to the paired `SubscriptionEvent` as its chat line. See resub note. |

## Ignored (fall through `_ => {}`) — captured shapes for future use

| Event | Why ignored | Payload shape (real capture) |
|---|---|---|
| `pusher:connection_established` | Pusher housekeeping | `{socket_id, activity_timeout}` |
| `pusher_internal:subscription_succeeded` | Pusher housekeeping | `{}` |
| `pusher:ping` / `pusher:error` / `pusher:subscription_error` | Handled separately (pong / error row) | — |
| `ChannelSubscriptionEvent` | Redundant sibling of `SubscriptionEvent` | `{user_ids[], username, channel_id}` |
| `PollUpdateEvent` | Polls not shown (no prediction/poll UI) | `{poll:{title, options:[{id,label,votes}], duration, remaining, result_display_duration}}` — fires ~1/sec while a poll runs (very chatty) |
| `PollDeleteEvent` | Poll not shown | `[]` |
| `GiftsLeaderboardUpdated` | Leaderboard not shown; fires alongside every `GiftedSubscriptionsEvent` | `{leaderboard[], weekly_leaderboard[], monthly_leaderboard[], gifter_id, gifter_username, gifted_quantity}` |
| `KicksLeaderboardUpdated` | Leaderboard not shown; fires alongside every `KicksGifted` | `{gifts_lifetime[], gifts_week[], gifts_month[], *_enabled}` |
| `ChatMoveToSupportedChannelEvent` | Raid-out redirect (host moving viewers); not acted on | `{channel:{...current_livestream...}, hosted:{username, viewers_count, is_live, ...}}` |
| `StreamHostedEvent` | Sibling of `StreamHostEvent` (which we do handle) | `{message:{numberOfViewers, optionalMessage}, user:{username}}` |

## The resub-message note (important)

A Kick (re)subscription fires **three separate Pusher frames**, in this order, for the same user:

1. `ChannelSubscriptionEvent` — `{user_ids, username, channel_id}`
2. `ChatMessageSentEvent` — `{message:{action:"subscribe", optional_message, months_subscribed}, user}`
3. `SubscriptionEvent` — `{chatroom_id, username, months}`

We render the sub off `SubscriptionEvent` (arm produces `Event{Sub}`) and **attach the resub's typed
message when present** (Twitch-style, rendered as a chat line under the sub text).

**The resub's typed message lives ONLY on `ChatMessageSentEvent.message.optional_message`**
(`SubscriptionEvent` never carries it). Since the message and the sub arrive on separate frames, the
connector buffers the `ChatMessageSentEvent`'s `optional_message` (keyed by lowercased username) and
pops it when the paired `SubscriptionEvent` arrives a moment later, building a `Message` via the
shared Kick builder so inline `[emote:id:name]` emotes render (`sub_message` in `connector.rs`). The
attached message carries no badges/color — the sub frame sends no identity block, matching a plain
sub line. If no `ChatMessageSentEvent` with text preceded the sub (the common case — nobody attaches
text), `message` is `None` and it renders as a bare sub line. `ChannelSubscriptionEvent` stays
ignored — it's the redundant third frame with no message.

## Chattiness warning

`PollUpdateEvent` (~1/sec per active poll), `GiftsLeaderboardUpdated`, and `KicksLeaderboardUpdated`
are the highest-volume ignored events. If any are ever handled, throttle/dedupe them — a live poll
alone produces 100+ frames.
