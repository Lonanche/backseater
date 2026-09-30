# PirateTok: locally patched copy

Backseater uses a **modified, vendored copy** of
[PirateTok/live-rs](https://github.com/PirateTok/live-rs), not the unmodified
published crate.

- Upstream version: `0.2.1`.
- Upstream commit: [`092930f7090650f161d85e13dce222bcc12b765e`](https://github.com/PirateTok/live-rs/tree/092930f7090650f161d85e13dce222bcc12b765e).
- License: [0BSD](LICENSE), retained unchanged; the original author remains credited
  in `Cargo.toml`.
- Consumer: [`crates/tiktok`](../../crates/tiktok), via a local Cargo path dependency.
- The original `src` tree, README and license were copied. CLI/example targets,
  optional CLI dependencies, and unrelated repository files are not part of this build.

## Windows build fix

Upstream's `build.rs` scans source files and compares paths against exemptions
written with forward slashes, such as `src/structs/proto/messages.rs` and `src/bin/`.
Windows returns paths with backslashes. Those comparisons therefore failed, causing
valid upstream proto/CLI files to fail the build script's line-count and
error-handling checks.

Our `scan_file` normalizes separators before comparing:

```rust
let rel = path.to_string_lossy().replace('\\', "/");
```

The checks and their limits remain enabled. This is a path compatibility fix;
we did not disable the source checks to make the Windows build pass.

## Other local patches

| File | Change and reason |
| --- | --- |
| `Cargo.toml` | Library-only manifest: `autobins = false`; removed upstream CLI/example target declarations, CLI feature and CLI-only optional dependencies. |
| `src/lib.rs`, `src/websocket/connection.rs` | Emit `Connected` after the WebSocket handshake and room-entry send succeed, instead of immediately after resolving a room ID. Avoids falsely reporting a failed connection as live. |
| `src/lib.rs`, `src/structs/events.rs` | Added `ConnectionError` so setup failures reach the application rather than appearing only in tracing logs. |
| `src/structs/events.rs`, `src/websocket/connection.rs` | Added chat/emote history variants and preserve the wire envelope's history flag. Drop historical non-chat events so old gifts/follows do not trigger new alerts. |
| `src/http/api.rs` | Percent-encode the username in room lookup requests and check HTTP error status before JSON parsing. |
| `src/structs/proto/messages_ext.rs` | Removed the assumed user-message field at subscription tag 11. Other schemas use a scalar enum there, which otherwise makes Prost reject the entire notice. Unused tag 11 is skipped; identity is read from tag 2. |

Backseater's adapter owns offline polling, retry backoff, fresh room resolution,
bounded duplicate suppression and cancellation. Its setup timeout stops once
connected; the library's socket stale timer handles established connections and
counts heartbeat traffic. These are adapter behaviors, not additional vendor patches.

## Updating upstream

Compare against the pinned commit before replacing this directory. Reapply or
explicitly retire each patch above, preserve the license, and update this file
with the new version/commit and patch list. Do not replace the path dependency
with the published crate until the needed fixes are present there.

Run from the repository root on Windows:

```sh
cargo test -p bks-tiktok
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --release -p backseater --locked
```

`crates/tiktok/tests/vendor_compat.rs` uses a local WebSocket server to check
connection event ordering, handshake failure, history flags, suppression of
historical gift alerts, and frame acknowledgements. Adapter tests cover quiet
connections, setup timeout, cancellation, duplicate suppression, subscription
wire compatibility, Unicode messages, badges, gifts and read-only behavior.
These tests do not contact TikTok or use saved accounts.

For a read-only live smoke test, choose a currently public LIVE channel:

```sh
cargo run -p bks-tiktok --example read_chat -- username 60
```

The earlier live check received 19 new chat messages and 6 history messages in
60 seconds, with no connector errors. This verifies that stream, not every event
variant or private/restricted room. Mixed text/emote placement is currently best
effort: attached emotes are displayed after the text because their wire index
semantics have not been confirmed. Emote-only messages retain their received order.
