// Kick OAuth token broker (Cloudflare Worker).
//
// Kick's OAuth requires a client *secret* for the token exchange, which must not
// ship in the desktop binary. This Worker holds the secret (as a Worker secret)
// and performs ONLY the two secret-requiring steps — exchanging an auth code and
// refreshing — on the desktop app's behalf. PKCE (`code_verifier`) is forwarded
// from the app, which is what proves the request is legitimate without exposing
// the secret to users.
//
// The app's anonymous Kick *reads* (channel/emotes/usercard/history) used to go
// through this Worker because Cloudflare 403s plain in-process Rust clients. They
// now run in-process via a browser-TLS-fingerprinting client (`wreq`), so the
// Worker is OAuth-only — it is NOT a general proxy.
//
// Security posture:
//   - Secret lives only in Worker env (`wrangler secret put KICK_CLIENT_SECRET`).
//   - Only `authorization_code` and `refresh_token` grants against Kick's fixed
//     token endpoint are allowed, plus a public `GET /kick/config` (client id).
//   - `redirect_uri` is fixed server-side so a caller can't redirect a code.
//   - POST for token/refresh; GET for config; everything else is 404/405.
//   - Tokens/secrets are never logged.

const TOKEN_URL = "https://id.kick.com/oauth/token";
// Must match the redirect registered on the Kick app and used by the desktop app.
const REDIRECT_URI = "http://localhost:38275";

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    // The client_id is public (it appears in the browser authorize URL), so the
    // app fetches it from here to keep all Kick-app config in the Worker.
    if (request.method === "GET" && url.pathname === "/kick/config") {
      if (!env.KICK_CLIENT_ID) {
        return json({ error: "broker_misconfigured" }, 500);
      }
      return json({ client_id: env.KICK_CLIENT_ID });
    }

    if (request.method !== "POST") {
      return json({ error: "method_not_allowed" }, 405);
    }

    let params;
    try {
      params = await readBody(request);
    } catch {
      return json({ error: "invalid_body" }, 400);
    }

    if (url.pathname === "/kick/token") {
      const code = str(params.code);
      const codeVerifier = str(params.code_verifier);
      if (!code || !codeVerifier) {
        return json({ error: "missing_code_or_verifier" }, 400);
      }
      return exchange(env, {
        grant_type: "authorization_code",
        redirect_uri: REDIRECT_URI,
        code,
        code_verifier: codeVerifier,
      });
    }

    if (url.pathname === "/kick/refresh") {
      const refreshToken = str(params.refresh_token);
      if (!refreshToken) {
        return json({ error: "missing_refresh_token" }, 400);
      }
      return exchange(env, {
        grant_type: "refresh_token",
        refresh_token: refreshToken,
      });
    }

    return json({ error: "not_found" }, 404);
  },
};

// Performs the token request to Kick with our client credentials added, and
// returns Kick's response verbatim. Only the fields we control are sent.
async function exchange(env, grantFields) {
  if (!env.KICK_CLIENT_ID || !env.KICK_CLIENT_SECRET) {
    return json({ error: "broker_misconfigured" }, 500);
  }

  const body = new URLSearchParams({
    client_id: env.KICK_CLIENT_ID,
    client_secret: env.KICK_CLIENT_SECRET,
    ...grantFields,
  });

  let resp;
  try {
    resp = await fetch(TOKEN_URL, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body,
    });
  } catch {
    return json({ error: "upstream_unreachable" }, 502);
  }

  // Pass through Kick's body + status so the app sees real OAuth errors.
  const text = await resp.text();
  return new Response(text, {
    status: resp.status,
    headers: { "content-type": "application/json" },
  });
}

// Accepts JSON or form-encoded bodies (we send JSON from the app).
async function readBody(request) {
  const ct = request.headers.get("content-type") || "";
  if (ct.includes("application/json")) {
    return await request.json();
  }
  const form = await request.formData();
  return Object.fromEntries(form.entries());
}

function str(v) {
  return typeof v === "string" && v.length > 0 ? v : null;
}

function json(obj, status = 200) {
  return new Response(JSON.stringify(obj), {
    status,
    headers: { "content-type": "application/json" },
  });
}
