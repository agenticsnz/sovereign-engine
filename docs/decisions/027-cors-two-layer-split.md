# ADR 027: CORS two-layer split for cookie-bearing and bearer routes

**Status:** Accepted
**Date:** 2026-04-29
**Related:** [ADR 019 — OIDC Callback URL](019-oidc-callback-url.md), [ADR 026 — Subdomain routing](026-subdomain-routing.md), gh#2

## Context

Sovereign Engine is a shared inference platform operated by a club or small organisation. Members run clients of their own choosing — server-side scripts, notebooks, local tools, and increasingly browser-based applications. A browser-based client may originate from any domain: the member's own web app, a development server, a prototype at `localhost`, or a third-party interface such as `claude.ai`.

Prior to 1.8.0, the proxy applied a single CORS layer at the outer router level:

```rust
AllowOrigin::list([api_external_url, chat_external_url])
allow_credentials(true)
```

This worked for the portal and OIDC flows (both origins are in the list) but silently blocked browser clients on any other origin. The browser received an empty `Access-Control-Allow-Origin` while other CORS headers were still emitted, producing the inconsistent-header symptom reported in gh#2. Server-side SDKs (Python, Node.js, etc.) are unaffected — they don't enforce CORS — so the problem was invisible in typical API testing but broke every browser-based member client on a non-listed origin.

### The two consumption shapes

The API has two route groups with fundamentally different threat models:

**Cookie-bearing routes** — `/auth/*`, `/api/*`, `/portal/*`, and the Open WebUI fallback. The session cookie (`se_session`) is a browser-managed credential. When JavaScript opts in with `credentials: 'include'`, the browser attaches it automatically on cross-origin requests. This is precisely the attack surface that CSRF exploits: a page on an attacker-controlled origin can trigger a state-changing request and the browser silently adds the cookie. The server must maintain a strict allow-list (`AllowOrigin::list(...)`) with `allow_credentials(true)` to limit which origins can receive authenticated responses.

**Bearer-token routes** — `/v1/*` (`/v1/chat/completions`, `/v1/completions`, `/v1/models`, `/v1/messages`). Authentication uses `Authorization: Bearer se-<uuid>` or `x-api-key`. Browsers do not attach these headers automatically; they are set explicitly by application JavaScript that already possesses the token. An attacker-controlled page cannot obtain the user's bearer token via cross-origin means — the same-origin policy prevents it from reading the token out of the legitimate app's memory or storage. CSRF is therefore structurally impossible against these routes regardless of which origin makes the request.

These two shapes have been conflated under a single CORS policy since the project's inception. The fix is to express them separately.

## Decision

Apply two distinct CORS layers, each attached at sub-router level rather than at the outer router.

| Layer | Routes | `AllowOrigin` | `allow_credentials` |
|---|---|---|---|
| **Strict** | `/auth/*`, `/api/*`, `/portal/*`, WebUI fallback | `AllowOrigin::list([api_external_url, chat_external_url])` | `true` |
| **Bearer** | `/v1/*` | `AllowOrigin::any()` | `false` |

The strict layer is unchanged from the pre-1.8.0 intent: only the two operator-configured origins (`API_HOSTNAME` and `CHAT_HOSTNAME`) can receive credentialed responses. This protects every route where the session cookie is the authentication mechanism.

The bearer layer opens to `AllowOrigin::any()`. This is safe because:

1. `/v1/*` accepts no cookies as auth — the bearer token must be explicitly supplied in a request header.
2. `allow_credentials(false)` ensures the browser will not attach cookies on these requests regardless of client intent (`credentials: 'include'` is silently ignored when the server sends `Access-Control-Allow-Credentials: false`).
3. A cross-origin attacker page can call `/v1/*` anonymously but cannot impersonate a user — it has no path to obtain the user's bearer token.

### Wiring

Each sub-router owns its CORS layer internally. No CORS layer is applied at the outer router level. This keeps the policy locally visible at each route group and prevents accidental double-wrapping. From `proxy/src/main.rs`:

- The API sub-router applies the strict layer to `auth_routes`, `api_routes`, and `portal_routes`, and the bearer layer to `v1_routes`.
- The chat sub-router applies the strict layer to the Open WebUI fallback.

## Consequences

- Members can build browser-based clients on any origin without operator intervention. There is no allow-list to maintain and no self-service request flow for the `/v1/*` surface.
- The bearer layer never sends cookies cross-origin. The `se_session` cookie carries `SameSite=Lax` (or `Strict`) and `allow_credentials(false)` is the explicit CORS-level signal.
- The strict layer's CSRF surface is now a single browser-auto-attached credential (the session cookie). HTTP Basic Auth was removed from all middleware in the same 1.8.0 release (see gh#2), reducing the credential surface from two to one.
- A cross-origin attacker page calling `/v1/*` can make anonymous requests but cannot impersonate a user. It has no mechanism to obtain or inject a valid bearer token.
- Operators lose passive visibility into which origins are calling `/v1/*`. Server-side logging of the `Origin` request header on `/v1/*` is a low-friction follow-up if origin tracking becomes important.
- **tower-http 0.6 quirk:** when `allow_credentials(true)` is set on the strict layer, `Access-Control-Allow-Credentials: true` appears on every preflight response, including those from non-matching origins. The browser still blocks the request correctly because `Access-Control-Allow-Origin` is absent. This is safe but worth noting when reading raw response headers.

## Alternatives Considered

**Wider but still-closed allow-list (`CORS_ALLOWED_ORIGINS` env var).** Operators could list additional origins at deploy time. Rejected because it does not satisfy the core consumption requirement: members must be able to build browser clients on member-chosen origins without waiting for operator intervention. A list that operators control still leaves members blocked by default.

**Self-service per-user origin registration.** Each member could register their allowed origins via the portal, and the server would reflect the matching origin per request. Rejected as unnecessary for the bearer surface — because `allow_credentials(false)` already makes any origin safe, per-user registration would add UX friction with no security gain. Could be revisited if operators want per-user origin auditing.

**`AllowOrigin::any()` on a single CORS layer applied uniformly.** This would require dropping `allow_credentials(true)`, which breaks OIDC sessions for cross-origin portal use. tower-http rejects the combination of `*` + `allow_credentials(true)` at runtime. Collapsing to a single permissive policy is not viable.

**Reflecting any `Origin` back unconditionally.** On the cookie routes this is a CSRF vector and was rejected outright. On the bearer routes it would achieve the same effect as `AllowOrigin::any()` but is harder to audit — a static wildcard is a clearer signal to both reviewers and browsers.

**Token-bound origin claims.** Each bearer token could carry a list of allowed origins; the server would enforce per-request that the `Origin` header matches. Acknowledged as orthogonal and potentially complementary (defence-in-depth against token theft). Not blocked by this ADR. Could be revisited if token-theft threat scenarios warrant the added complexity.
