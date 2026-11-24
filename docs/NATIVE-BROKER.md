# Native stream broker (RAST Desktop, Phase 3)

RAST Desktop attaches the **official native Xpra client** to a person's shared
Linux graphical session instead of embedding the HTML5 viewer. That client is
not a browser: it has no `rast_session` cookie jar and sends no `Origin`
header, so the HTML5 proxy's authorization model (cookie + same-origin) cannot
apply to it. This document covers the additive server mechanism that replaces
it: a short-lived, single-use **stream token** broker and an authenticated
native WebSocket relay.

The wire contract is defined in the RAST Desktop repository,
`docs/PROTOCOL.md` §2 and §4; this document describes the server side.

```
RAST Desktop (macOS)                     Linux server (RAST 0.19+)
  │ POST /api/linux-apps/session/stream-token      (cookie + CSRF)
  │──────────────────────────────────────────────────────────────▶ issued
  │◀───────── {token, url: wss://…/linux-apps/native/<token>/} ──── (120 s, single use)
  │
  │ GET /linux-apps/native/<token>/<rest>?<query>  Upgrade: websocket
  │──────────────────────────────────────────────────────────────▶ native relay
  │◀═══════════════ binary Xpra frames (transparent relay) ═════════│  loopback only
                                                                   ▼
                                            ws://127.0.0.1:<xpra-port>/<rest>?<query>
```

No new public port is opened: the relay is served by the same HTTPS listener
(and the same reverse proxy path) as the rest of RAST. Xpra keeps listening on
loopback only.

## Endpoints

### Issue a token

```
POST /api/linux-apps/session/stream-token
Cookie: rast_session=…            (required: ordinary RAST login)
X-CSRF-Token: …                   (required, like every mutating API route)
{}                                (no body required)

→ 200
{
  "protocol": 1,
  "token": "<43-char base64url, 32 random bytes>",
  "url": "wss://desktop.example.com/linux-apps/native/<token>/",
  "expiresAt": 1795550000000,
  "ttlSeconds": 120,
  "transport": "xpra-websocket",
  "session": { … NativeSession … },
  "xpra": {"sharing": true, "protocol": 1}
}
```

* The token is **single use**: it is consumed by the first successful
  WebSocket upgrade. Obtain a fresh token for every (re)connect.
* TTL: 120 seconds (constant `STREAM_TOKEN_TTL`).
* Bound to the caller's account id, the RAST login session that requested it
  **and** the exact native session (`session.key`). A token cannot be used for
  another account, becomes useless when the session it names is gone, and the
  stream is revalidated against the login session every 5 s.
* At most 8 outstanding tokens per account; the oldest is evicted.
* The URL is derived from the configured `public_url` (HTTPS becomes `wss://`)
  when set; otherwise from the request `Host` plus `X-Forwarded-Proto`, falling
  back to the `secure_cookies` setting. Only scheme and authority are used:
  RAST must be served at the root of its host (as the rest of the API assumes).

Errors (standard RAST error body):

| Status | Meaning |
| --- | --- |
| 401 | not signed in / session expired |
| 403 | CSRF mismatch, cross-origin request, account inactive |
| 404 | the caller has no live native session (launch/pre-warm first) |
| 409 | native Linux apps are disabled on this server |
| 429 | issuance rate limit (`Retry-After`); 30 requests/minute/account |

### Revoke outstanding tokens

```
DELETE /api/linux-apps/session/stream-tokens
Cookie: rast_session=…            X-CSRF-Token: …

→ 200 {"revoked": 3}
```

The desktop client calls this on logout/detach best-effort. Tokens also die
with their TTL, with the native session, with a server restart, and when the
RAST login session is revoked (a live stream is closed within 5 s).

### Native WebSocket attach

```
GET /linux-apps/native/<token>/<optional-rest>?<query>
Upgrade: websocket
```

* The token in the path **is the credential**. This route is deliberately
  mounted outside cookie authentication and outside the browser same-origin
  `Origin` check (native clients are not browsers and can present neither). It
  is not under `/api`; the rest of the public API is unchanged.
* The server validates and consumes the token atomically before returning the
  `101`, then relays frames transparently, both directions, to
  `ws://127.0.0.1:<xpra-port>/<rest?query>`. The token prefix is stripped, the
  client's remainder and query are forwarded verbatim, and the client's
  `Sec-WebSocket-Protocol` offer is negotiated end to end with Xpra.
* Frame limits and semantics are the same as the browser relay: 64 MiB maximum
  message, ping/pong/close relayed, viewer/input bookkeeping for the session's
  idle rules.
* Invalid/expired/consumed/mismatched tokens get a bodyless `401` (the response
  never echoes the token) and the connection is closed.
* A non-WebSocket request to the route gets `401` and does not consume the
  token.
* A client address doing more than 60 attach attempts per minute gets `429`.

## Client usage

The token URL is a short-lived secret: use it for one connection, do not write
it to disk or logs.

```bash
# 1. Obtain a token (the URL contains the token - do not log or share it).
curl -sS -b cookies.txt -c cookies.txt \
  -H 'X-CSRF-Token: '"$CSRF" \
  -X POST https://desktop.example.com/api/linux-apps/session/stream-token
# → {"protocol":1,"token":"…","url":"wss://…/linux-apps/native/…/", …}

# 2. Attach the official native Xpra client.
#    XPRA_WEBSOCKET_MASK=1 is MANDATORY: xpra 6.x defaults to sending UNMASKED
#    WebSocket frames, which RFC 6455 forbids for clients; the relay is strict
#    and closes such a connection right after the hello (the client then says
#    "0 packets received ... failed to receive anything"). Xpra's own
#    --bind-ws server tolerates it, which is why a direct test can pass.
XPRA_WEBSOCKET_MASK=1 /path/to/Xpra.app/Contents/MacOS/Xpra attach "$URL" \
  --sharing=yes --clipboard=yes --clipboard-direction=both \
  --audio=yes --speaker=on --microphone=no --exit-with-client=no
```

Browser JavaScript (for a server-side integration check; the desktop client is
Rust and uses the same protocol):

```js
const token = await fetch('/api/linux-apps/session/stream-token', {
  method: 'POST',
  credentials: 'same-origin',
  headers: {'X-CSRF-Token': csrf},
});
const {url} = await token.json();
const ws = new WebSocket(url, ['binary']);
ws.binaryType = 'arraybuffer';
ws.onmessage = ({data}) => { /* Xpra binary frames */ };
```

## Security model

* **One narrow credential.** The attach route accepts exactly one thing: a
  256-bit random token issued by an authenticated, CSRF-protected API call. No
  cookie, no `Origin`, no session key is accepted there.
* **Hash-only storage, no logs.** The server keeps only `SHA-256(token)` in
  memory (never the token itself, never a database row). The token, the issued
  URL, the native session key and the `rast_session` cookie never appear in
  server logs or audit entries. The route sets `Cache-Control: no-store` and
  never echoes the token in a response body.
* **Single use, short TTL.** The token is consumed atomically before the
  `101`; a replayed URL connects to nothing after the legitimate client
  attached, and dies after 120 s in any case.
* **Bound to account + login session + native session.** The token names one
  user and one session key; the relay resolves the live session by that key,
  checks the owner, requires a live loopback Xpra port, and re-checks the
  issuing RAST login session at attach time and every 5 s thereafter (logout,
  revocation, expiry and account deactivation all close the stream). The
  session's own teardown closes it too.
* **Loopback only.** The relay is a WebSocket *client* to
  `ws://127.0.0.1:<xpra-port>`. Xpra is never exposed and no new listener is
  opened; the native client reaches RAST through the existing HTTPS/WSS
  reverse-proxy path only. TLS is mandatory in production (the issued URL is
  `wss://`).
* **RFC 6455 strictness.** The relay requires masked client frames, as the
  RFC mandates. Xpra must be started with `XPRA_WEBSOCKET_MASK=1`; unmasked
  frames are a protocol error and the connection is closed cleanly (pinned by
  `an_unmasked_client_frame_is_closed_strictly`).
* **Bounded and rate limited.** 8 outstanding tokens per account (oldest
  evicted), 30 issuances/minute/account, 60 attach attempts/minute/address;
  expired grants are pruned on every operation. Guessing is pointless against
  256 random bits, and the attach limit keeps denial responses from becoming an
  audit-log write amplifier.
* **The browser path is untouched.** The HTML5 proxy still enforces cookie
  auth and same-origin `Origin` on every request; it shares only the frame
  relay implementation (same 64 MiB cap, now with an optional, off-by-default
  diagnostics switch). No existing route or response changed.

## Audit events

Written to the existing activity log with no secrets:

| Event | When | Contents |
| --- | --- | --- |
| `linux_apps.stream_token.issued` | a token was issued | user id, session kind (`native`), `ttl=120s` |
| `linux_apps.stream_token.denied` | an attach was refused | user id when known, static reason category (e.g. `expired token`, `session is gone`) |
| `linux_apps.stream.attached` | an attach succeeded | user id, session kind, remote address **class** (`loopback`/`private`/`public`), never the exact address |

The token, URL, session key and cookie are never part of an audit row.

## Operations

* **Reverse proxy:** no new port and no new `location` are required. The
  existing WebSocket upgrade configuration must cover `/linux-apps/` (or `/`);
  for nginx that is `proxy_set_header Upgrade $http_upgrade;` and
  `proxy_set_header Connection $connection_upgrade;` at the `location` (see
  `docs/INSTALL.md` / `docs/OPERATIONS.md`).
* **Access logs:** the attach URL contains the short-lived token in its path.
  Keep the reverse proxy's access log disabled for `/linux-apps/` — the same
  rule already documented for the HTML5 session key — so the token cannot leak
  into proxy logs. `Cache-Control: no-store` is set on relay responses.
* **Diagnostics:** set `RAST_NATIVE_RELAY_DEBUG=1` in the RAST service
  environment to get info-level trace of native attaches: loopback target
  path/query, upstream handshake result and negotiated sub-protocol, frame
  counts/sizes per direction, close reasons and connection finish. Frame
  contents, the token, the URL and cookies are never logged. Unset it after
  diagnosing; it does not affect the browser relay.
* **Restart:** tokens are in memory, so a RAST restart invalidates all of them;
  the desktop core reconnects with a fresh token (backoff 1 s → 2 s → 5 s →
  15 s, max 5 attempts while the session exists).

## Tests

`cargo test --locked --lib native_broker` covers:

* 32-byte base64url token format, CSPRNG uniqueness, hash-only storage;
* TTL expiry, single-use (including an 8-way concurrent consume race);
  account + login session + native session binding;
* max 8 outstanding per user with oldest-eviction, per-user isolation;
  revoke-all per user; per-account issuance rate limiting;
* `public_url` / `X-Forwarded-Proto` / `Host` URL derivation;
* attach-path prefix stripping and rest+query preservation;
* bootstrap `desktop` capability JSON;
* route ranking: `/linux-apps/native/…` never falls into the legacy
  `/linux-apps/{*rest}` proxy;
* a mock-Xpra tokio test: sub-protocol negotiated end to end, an upstream frame
  sent immediately after handshake is delivered, a masked 5240-byte first
  client frame is relayed byte-for-byte, path+query preserved, a second attach
  with the consumed token is refused, viewer accounting works;
* unmasked client frames are closed strictly (the Xpra `XPRA_WEBSOCKET_MASK=1`
  requirement);
* native client frames count as session activity (idle rules).

There is also an `#[ignore]`d end-to-end test against the **real** Xpra
protocol (`xpra info` as a full WebSocket client) that is run on demand on a
host with xpra installed:

```bash
# A throwaway Xpra server on loopback (never exposed):
xpra start :199 --daemon=no --bind-ws=127.0.0.1:18999 \
  --start-new-commands=no --notifications=no --mdns=no --pulseaudio=no &
# The real client attaches through the new broker route:
RAST_TEST_REAL_XPRA_WS=127.0.0.1:18999 \
  cargo test --locked --lib a_real_xpra_protocol_client -- --ignored
```

It asserts the real client receives Xpra server data through the relay, the
relay counts the viewer, the token is consumed by that attach and a second
attach with the same URL is refused. Verified against Xpra 6.5.5 on
2026-11-24 (Linux, loopback).
