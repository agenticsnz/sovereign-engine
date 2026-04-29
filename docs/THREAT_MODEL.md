# Sovereign Engine — Threat Model

**Date:** 2026-02-17
**Related:** [Security Assessment](../SECURITY_ASSESSMENT.md) (detailed audit with remediation log)

This document maps the attack surfaces of a Sovereign Engine deployment and shows how each threat is addressed by the architecture.

---

## System Trust Boundaries

```
┌─────────────────────────── Host ─────────────────────────────┐
│                                                              │
│  ┌──────────── sovereign-public ───────────────────────┐     │
│  │                                                     │     │
│  │  ┌──── Proxy (Rust/axum) ──────────────────────┐   │     │
│  │  │  TRUST BOUNDARY: Docker socket access        │   │     │
│  │  │  TRUST BOUNDARY: All auth decisions          │   │     │
│  │  │  TRUST BOUNDARY: Secrets in memory           │   │     │
│  │  └──────────────┬──────────────────────────────┘   │     │
│  │                 │                                   │     │
│  └─────────────────┼───────────────────────────────────┘     │
│                    │                                          │
│  ┌──── sovereign-internal (isolated, no host access) ───┐    │
│  │                 │                                    │    │
│  │    ┌────────────┴───────────┐                        │    │
│  │    │  Backend containers    │  No port bindings       │    │
│  │    │  Unique UIDs           │  Read-only /models      │    │
│  │    │  Per-container API key │  No internet access     │    │
│  │    └────────────────────────┘                        │    │
│  └──────────────────────────────────────────────────────┘    │
│                                                              │
│  Docker socket (/var/run/docker.sock) — root-equivalent      │
└──────────────────────────────────────────────────────────────┘
```

---

## Threat Matrix

### Network & Infrastructure

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| N1 | **Direct backend access** — attacker bypasses proxy and queries llama.cpp directly | Backends on `internal: true` network, no host port bindings. Only proxy is dual-homed. | **Eliminated** |
| N2 | **Container lateral movement** — compromised backend attacks another backend | Unique unprivileged UIDs per container, read-only model mounts, no `--privileged` flag, no dangerous capabilities. Proxy itself runs as non-root (`sovereign` user). | **Mitigated** |
| N3 | **Proxy↔backend eavesdropping** — traffic sniffed on internal network | Isolated network (only proxy + backends), per-container API keys as defence-in-depth | **Accepted** (encrypted internal traffic is over-engineering for single-host) |
| N4 | **Docker socket compromise** — proxy is compromised, attacker controls Docker API | Architectural trust boundary. Mitigated by Rust memory safety, parameterised queries, input validation. Defence-in-depth: rootless Docker or docker-socket-proxy. | **Documented** |
| N5 | **MITM / eavesdropping on client traffic** | TLS via rustls (manual certs or ACME Let's Encrypt), HSTS header (1 year) | **Mitigated** |

### Authentication

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| A1 | **Session hijacking** — attacker steals session cookie | HttpOnly (no JS access), SameSite=Lax (no cross-site), Secure flag (no HTTP), SHA-256 hashed in DB, 24h TTL, hourly cleanup. Cookie `Domain` attribute set to parent domain for cross-subdomain sharing when `COOKIE_DOMAIN` is configured. | **Mitigated** |
| A2 | **API token theft** — attacker obtains `se-{uuid}` token | SHA-256 hashed in DB (irreversible), 90-day default expiry, revocation supported, scoped to model/category | **Mitigated** |
| A3 | **OIDC flow manipulation** — CSRF, replay, code injection | PKCE (SHA-256), random CSRF token, random nonce (verified in ID token), 10-minute state expiry, no HTTP redirects on OIDC client | **Mitigated** |
| A4 | **Bootstrap brute force** — attacker guesses BOOTSTRAP_PASSWORD | Disabled by default (`BREAK_GLASS=false`), intended for initial setup only. `POST /auth/bootstrap-login` rate-limited to 5 attempts/min/IP. Constant-time comparison prevents timing side-channel. | **Mitigated** |
| A5 | **Privilege escalation** — user becomes admin | `admin_only_middleware` checks `is_admin` flag from DB. No client-side role switching. Admin flag set only by DB mutation or first-user auto-promotion. | **Eliminated** |
| A6 | **First-user auto-promotion** — attacker completes first OIDC login before operator | Intentional for single-operator deployment. Operator should complete OIDC login immediately after configuring IdP via bootstrap auth. | **Documented** |

### Authorization & Access Control

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| Z1 | **Token scope bypass** — token scoped to category A accesses category B | Scheduler enforces `specific_model_id` → `category_id` → request body chain. Category-scoped tokens bail with an error if no models are available in the category (no fallthrough to unrestricted resolution). | **Eliminated** |
| Z2 | **Reservation bypass** — non-holder accesses GPU during exclusive reservation | Active reservation checked on every inference request and WebUI proxy request. Non-holders receive 503. State persisted in DB + in-memory cache. | **Mitigated** |
| Z3 | **Open WebUI identity spoofing** — attacker injects `X-SE-User-*` headers | Proxy strips all incoming `X-SE-*` headers before forwarding. Only proxy-injected headers (from validated session) reach Open WebUI. | **Eliminated** |

### Data Protection

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| D1 | **DB theft → token compromise** | API tokens and session tokens stored as SHA-256 hashes. 128-bit entropy makes brute-force infeasible. | **Mitigated** |
| D2 | **DB theft → IdP secret compromise** | IdP client secrets encrypted with AES-256-GCM at rest (key from `DB_ENCRYPTION_KEY` env var). Random nonce per encryption. Plaintext auto-migrated on startup. | **Mitigated** |
| D3 | **Secret leakage in logs** | Structured logging via `tracing` — token names logged, never values. No env var values logged. `RUST_LOG` controls verbosity. | **Mitigated** |
| D4 | **Secrets in Docker inspect** | Container API keys visible via `docker inspect` to Docker socket holders. Acceptable: only the proxy has socket access in normal deployments. | **Low risk** |
| D5 | **Secrets in git** | `.gitignore` covers `.env`, `*.pem`, `*.key`, `*.crt`, `*.db`. `.env.example` uses placeholders. No secrets found in tracked files or git history. | **Eliminated** |

### Input Validation & Injection

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| I1 | **SQL injection** | All queries parameterised via sqlx with compile-time verification. No string concatenation of SQL anywhere. | **Eliminated** |
| I2 | **XSS** | No `dangerouslySetInnerHTML` or `innerHTML`. CSP with script-src hash whitelist. No tokens in localStorage. HttpOnly cookies. | **Mitigated** |
| I3 | **Path traversal** | Model paths derived from HuggingFace repo names (sanitised). HuggingFace download file paths validated to reject `..` and absolute paths. No user-supplied file paths in API. Backend model mounts are read-only. | **Eliminated** |
| I4 | **Input size DoS** | Centralised `validate_len` helper: MAX_NAME=256, MAX_URL=2048, MAX_DESCRIPTION=4096, MAX_SECRET=4096. Applied to all input-accepting handlers. Global 10 MB request body limit. | **Mitigated** |

### HTTP Security

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| H1 | **Clickjacking** | `X-Frame-Options: DENY` on all responses | **Eliminated** |
| H2 | **MIME sniffing** | `X-Content-Type-Options: nosniff` on all responses | **Eliminated** |
| H3 | **Permissive CORS** | Two-layer split (1.8.0): strict allow-list (`api.*` + `chat.*` origins, `allow_credentials(true)`) for `/auth/*`, `/api/*`, `/portal/*`, and WebUI; `AllowOrigin::any()` with `allow_credentials(false)` for `/v1/*` (bearer-only, CSRF-immune — browsers do not auto-attach `Authorization: Bearer` or `x-api-key`). HTTP Basic auth has been removed from all middleware, eliminating the second browser-auto-attached auth path. | **Mitigated** |
| H4 | **Missing CSP** | `Content-Security-Policy` with `script-src 'self' sha256-...` (hashes computed from index.html at startup). `style-src 'self' 'unsafe-inline'` (required by Vite). | **Mitigated** |
| H5 | **Referrer leakage** | `Referrer-Policy: strict-origin-when-cross-origin` on all responses | **Mitigated** |
| H6 | **Unnecessary browser features** | `Permissions-Policy: camera=(), microphone=(), geolocation=(), payment=()` on all responses | **Mitigated** |

### Denial of Service & Fair Use

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| F1 | **GPU monopolisation** — single user sends thousands of requests | Logarithmic fair-use scheduler penalises heavy usage. Per-model concurrency gates. Queue timeout (default 30s) returns 429 with Retry-After. | **Mitigated** |
| F2 | **Queue starvation** — low-priority user never gets served | Wait-time bonus in priority formula prevents starvation. Priority increases with wait time. | **Mitigated** |
| F3 | **Reservation abuse** — user reserves all available time | Admin approval required before reservation activates. Admins can reject, cancel, or force-deactivate. | **Mitigated** |

### Supply Chain

| # | Threat | Mitigation | Status |
|---|--------|------------|--------|
| S1 | **Malicious dependency** | Minimal dependency set. Security-critical paths use pure-Rust (rustls, sqlx, bollard — no C bindings). Versions pinned. Lock files committed. | **Mitigated** |
| S2 | **Compromised base image** | Docker image versions pinned (no `:latest`). Multi-stage build removes build tools from runtime image. Minimal runtime: `ca-certificates`, `curl`, `libssl3`. | **Mitigated** |

---

## Design Rationale — Why This Architecture

Several architectural choices address multiple threats simultaneously and were chosen for strategic reasons beyond individual mitigations:

### OIDC as the sole production auth mechanism

- **Eliminates password management entirely** — no password database, no password reset flows, no password policy enforcement. All of that is the IdP's problem.
- **MFA is delegated** — the IdP handles multi-factor authentication. Sovereign Engine gets the security benefits without implementing or maintaining MFA.
- **Rate limiting on auth is the IdP's responsibility** — brute-force protection, account lockout, and suspicious login detection are handled by mature IdP infrastructure (Azure Entra, Okta, Keycloak).
- **SSO with existing accounts** — in enterprise deployments, users authenticate with their existing Azure Entra / Google Workspace / Okta accounts. No new credentials to manage. Guest accounts in a new tenancy work seamlessly.
- **Break-glass login exists only for initial IdP configuration** — disabled by default, intended to be turned off immediately after setup. It is a one-shot form at `/portal/` (not HTTP Basic auth on every request) and is rate-limited.

### Separate API tokens per application

- **Scoped access** — each token can be restricted to a specific model or category. An application granted a "coding" token cannot access "thinking" models.
- **Independent revocation** — compromising one application's token doesn't affect others. Revoke a single token without disrupting other integrations.
- **Usage attribution** — per-token usage logging enables tracking which application consumes what resources, independent of the user who created the token.
- **No credential sharing** — applications never see the user's OIDC session. The user's identity and the application's access are fully decoupled.

### Per-container UIDs and API keys

- **Prevents privilege escalation** — a compromised container runs as an unprivileged user (UID 10000–65000). It cannot `su` to root or access other containers' processes.
- **Prevents horizontal movement** — each container has a unique UID and unique API key. Compromising one container's key does not grant access to another container's API.
- **Read-only model mounts** — a compromised container cannot modify model weights (e.g., to inject a backdoor into a model file).

### Open WebUI on an isolated network

- **No internet access** — Open WebUI cannot phone home, download plugins, or be used as a proxy to external services. The `internal: true` flag blocks all outbound traffic.
- **No direct host access** — Open WebUI has no port bindings. It is only reachable via the proxy, which enforces authentication and strips/re-injects identity headers.
- **Prevents horizontal movement** — even if Open WebUI is compromised, the attacker is trapped on the isolated network with no route to the host or internet.

### Fair-use scheduling instead of rate limiting

- **Prevents monopolisation without hard caps** — a user submitting many requests is not rejected outright; they are deprioritised. This is friendlier than rate limiting for legitimate burst usage.
- **Logarithmic penalty** — using 2x more tokens barely affects priority; using 100x more tokens significantly deprioritises. This matches real-world fairness intuitions.
- **Wait-time bonus prevents starvation** — even the lowest-priority user will eventually be served as their wait time increases their effective priority.
- **Runtime-tunable** — operators can adjust fairness parameters without recompilation via `/api/admin/settings`.

### Reservations for isolation and dedicated use

- **Exclusive-use guarantee** — during a reservation window, only the reservation holder can access inference. Other users receive 503, not degraded service.
- **Admin approval gate** — prevents users from reserving all available time. Admins control who gets exclusive access and when.
- **Automatic transitions** — reservations activate and deactivate on schedule (30s tick). No manual intervention required for time-based access control.

---

## Accepted Risks

| Risk | Rationale |
|------|-----------|
| Docker socket = root-equivalent | Fundamental architectural trust boundary. Proxy is the only consumer. Rust memory safety + input validation reduce exploit surface. |
| Backend traffic unencrypted | Internal-only network with no host/internet access. Per-container API keys add defence-in-depth. mTLS would add complexity disproportionate to threat. |
| No application-layer rate limiting on OIDC endpoints | OIDC auth happens at the IdP (their responsibility). The `/auth/callback` endpoint receives a one-time code. `POST /auth/bootstrap-login` is rate-limited (5/min/IP). API tokens are UUID v4 (122 bits of entropy) — brute force is infeasible. Session tokens are 256 bits of randomness. For network-exposed deployments, operators should use a reverse proxy (nginx, Cloudflare) for additional rate limiting. |
| No HTTP→HTTPS redirect | Port 80 not exposed in production. HSTS set. Adding port 80 listener increases attack surface. |
| 24h session TTL | Reasonable for local system. Shorter TTL would cause auth fatigue without proportional security gain. |
| First OIDC user auto-promoted to admin | Intentional for single-operator deployment. Operator completes first login immediately after IdP setup. |
| Logs not cryptographically signed | Single-tenant system. Use log aggregation (Docker logging driver, ELK) for tamper-evident audit. |
| GPU device passthrough (`/dev/dri`, `/dev/kfd`) | Backend containers receive the full `/dev/dri` directory (all render nodes). GPU VRAM is not namespaced — a compromised container could theoretically read residual GPU memory from other containers or exploit GPU driver kernel bugs. Mitigated by: unprivileged UIDs, no `--privileged`, isolated network, read-only model mounts. Single-tenant system where admin controls loaded models. Future: pass individual render nodes per-GPU for multi-GPU isolation. |

---

## Operator Recommendations

1. **Enable TLS** — use ACME or manual certificates. Never run without TLS on untrusted networks.
2. **Set `DB_ENCRYPTION_KEY`** — generate a strong 32+ character random key. Without it, IdP client secrets are stored plaintext.
3. **Disable bootstrap** — set `BREAK_GLASS=false` after initial setup.
4. **Use a trusted IdP** — Azure Entra, Okta, Keycloak, etc. with strong password policies and MFA.
5. **Aggregate logs** — capture stdout/stderr via Docker logging driver for persistent audit trail.
6. **Monitor dependencies** — watch Rust crate advisories (`cargo audit`) and Docker image CVEs.
7. **Restrict Docker socket** — consider rootless Docker or [docker-socket-proxy](https://github.com/Tecnativa/docker-socket-proxy) to limit API surface.
8. **Encrypt backups** — the SQLite database contains hashed tokens and encrypted secrets, but other data (user emails, model names) is plaintext.
