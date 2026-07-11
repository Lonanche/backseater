# Kick broker (Cloudflare Worker)

Kick's OAuth requires a client **secret** for the token exchange, which must not
ship in the desktop binary. This Worker holds the secret server-side and performs
only the two secret-requiring steps — exchanging an auth code and refreshing —
on the app's behalf. PKCE (`code_verifier`, generated per-login by the app) is
what authenticates the request; the secret never reaches users.

It does **not** proxy Kick's read endpoints. The app's anonymous reads
(channel/emotes/usercard/history) used to route through this Worker because
Cloudflare fingerprints the TLS handshake and 403s every in-process Rust client.
They now run in-process using a browser-TLS-fingerprinting client (`wreq`), which
passes Cloudflare directly — so the Worker is OAuth-only.

## Endpoints

- `GET  /kick/config`  — returns `{ "client_id": "..." }` (public; the app needs
  it to build the browser authorize URL).
- `POST /kick/token`   — body `{ "code": "...", "code_verifier": "..." }`
- `POST /kick/refresh` — body `{ "refresh_token": "..." }`

The two POST endpoints return Kick's token response verbatim. Anything else is
404/405. This is **not** a general proxy: only the two grant types above against
Kick's fixed token endpoint (with `redirect_uri` pinned server-side).

## One-time setup

1. Register a Kick app at <https://kick.com/settings/developer> with redirect URL
   `http://localhost:38275` and scopes `user:read channel:read chat:write moderation:ban`.
2. Install Wrangler and log in: `npm i -g wrangler && wrangler login`.
3. Store the app credentials as Worker **secrets** (never commit them):

   ```sh
   cd worker
   wrangler secret put KICK_CLIENT_ID
   wrangler secret put KICK_CLIENT_SECRET
   ```

4. Deploy:

   ```sh
   wrangler deploy
   ```

   Note the URL it prints, e.g. `https://chat-kick-broker.<account>.workers.dev`.

## Point the app at it

The app ships with a default broker URL baked in (`DEFAULT_BROKER_URL` in
`crates/auth/src/kick.rs` — not a secret), so `/kicklogin` works out of the box.
To use your own deployment, set `BKS_KICK_BROKER_URL` to its base URL (no
trailing slash):

```sh
BKS_KICK_BROKER_URL=https://chat-kick-broker.<account>.workers.dev cargo run -p backseater
```

The app does the browser login locally and calls the broker only for the
token exchange/refresh. No Kick client id/secret is needed on the user's machine.

## Security notes

- The secret exists only as a Worker secret (`wrangler secret put`), not in this
  repo or the binary.
- The Worker logs nothing — no tokens, codes, or secrets.
