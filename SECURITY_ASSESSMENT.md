# Sovereign Engine — Security Assessment

**Version:** 1.0.0
**Date:** 2026-02-18
**Scope:** Full-stack review — Rust proxy, React UI, Docker infrastructure, dependencies

---

## 1. Architecture Overview

Sovereign Engine is a self-contained local AI inference platform. A Rust reverse proxy (axum) manages llama.cpp containers, authenticates users via OIDC, schedules inference fairly, and serves a React dashboard. The entire system runs as a single Docker Compose deployment.

```
Internet
   │
   ▼
┌──────────────────────────────┐
│  Rust Proxy (axum/rustls)    │  ← TLS termination, auth, scheduling
│  Port 443 (TLS) or 3000     │
│  sovereign-public network    │
└──────┬───────────┬───────────┘
       │           │
       ▼           ▼
┌────────────┐ ┌──────────────────┐
│  React UI  │ │  sovereign-internal network (isolated)  │
│  (static)  │ │  ┌──────────┐ ┌──────────┐             │
└────────────┘ │  │ llama.cpp│ │ Open     │             │
               │  │ UID 10001│ │ WebUI    │             │
               │  │ :8080    │ │ :8080    │             │
               │  └──────────┘ └──────────┘             │
               └────────────────────────────────────────┘
```

Backend containers have **no host port bindings** and are reachable only via the proxy on the internal network.

---

## 2. Threat Protections Implemented

### 2.1 Lateral Movement Prevention — Backend Container Isolation

**Threat:** Compromised model runner pivots to other services or the host.

**Mitigations:**
- Each llama.cpp container receives a **randomly generated UUID API key** (`se-{uuid}`) at creation time. This key is never exposed to end users — it is injected by the proxy when forwarding requests. (`api/admin.rs:870`, `proxy/streaming.rs:30-31`)
- Backend containers run on `sovereign-internal`, a Docker bridge network with `internal: true` — **no outbound internet access, no host port bindings**. (`docker-compose.yml`, `docker/mod.rs:57-75`)
- Open WebUI is on the same isolated network, communicating only with the proxy via trombone routing. The proxy strips and re-injects identity headers (`X-SE-User-Email`, `X-SE-User-Name`) to prevent spoofing. (`proxy/webui.rs:129-156`)
- Incoming requests with `X-SE-*` headers are **stripped before forwarding** — only the proxy's authenticated values are injected. (`proxy/webui.rs:133-141`)
- Hop-by-hop headers are stripped per RFC 2616. (`proxy/webui.rs:17-24`)

### 2.2 Container Privilege Separation

**Threat:** Container escape or privilege escalation.

**Mitigations:**
- Each model runner runs as a **unique unprivileged UID** (allocated from 10000–65000 range with collision avoidance). No container runs as root. (`docker/llamacpp.rs:133-134`, `docker/mod.rs:107-140`)
- Model files are mounted **read-only** (`/models` bind with `read_only: true`). A compromised container cannot modify model weights or inject backdoors. (`docker/llamacpp.rs:150-152`)
- Containers run **without `--privileged`** and no dangerous capabilities (no `CAP_SYS_ADMIN`, `CAP_NET_ADMIN`, etc.).
- GPU access is scoped: NVIDIA uses `device_requests` (not raw device mounts); ROCm mounts only `/dev/kfd` and `/dev/dri` with explicit permissions. (`docker/llamacpp.rs:170-194`)

### 2.3 Memory Safety — Rust

**Threat:** Buffer overflows, use-after-free, and other memory corruption in the proxy.

**Mitigations:**
- The entire proxy is written in **Rust**, providing compile-time memory safety guarantees without garbage collection overhead.
- TLS is handled by **rustls** (0.23), a pure-Rust TLS implementation — no OpenSSL/C memory risks. (`tls.rs`, `Cargo.toml`)
- Docker API interaction uses **bollard** (safe Rust bindings), not shell exec.
- Database queries use **sqlx** with compile-time SQL verification — **no SQL injection possible** via parameterised queries.

### 2.4 TLS / Transport Security

**Threat:** Eavesdropping, MITM, credential interception.

**Mitigations:**
- **Three TLS modes:** manual certificate files, automatic Let's Encrypt (ACME TLS-ALPN-01), or HTTP-only for development. (`tls.rs:14-62`, `config.rs:116-137`)
- ACME certificates are **automatically provisioned and renewed** with built-in state machine. (`tls.rs:46-54`)
- ACME staging mode available via `ACME_STAGING` for testing without rate limits. (`tls.rs:40`)
- Certificate/key files excluded from git (`.gitignore`: `*.pem`, `*.key`, `*.crt`).

### 2.5 Authentication & Authorization

**Threat:** Unauthorised access to inference APIs or admin functions.

**Mitigations:**
- **OIDC (OpenID Connect)** with full PKCE flow (SHA-256 challenge/verifier), CSRF token validation, and nonce verification to prevent replay attacks. (`auth/oidc.rs:110-118, 197, 238-250`)
- OIDC HTTP client **disables redirects** to prevent SSRF against the IdP discovery/token endpoints. (`auth/oidc.rs:445-446`)
- OIDC auth state (CSRF, nonce, PKCE verifier) stored in DB with **10-minute expiry** and cleaned after callback. (`auth/oidc.rs:489-516, 298`)
- **API tokens** use `se-{uuid}` format, stored as **SHA-256 hashes** — plaintext tokens are never persisted. (`auth/tokens.rs:11-20`)
- **Session tokens** are random 32-byte hex, also stored as SHA-256 hashes with 24-hour TTL. (`auth/sessions.rs:11-20`)
- Token **revocation** is supported and checked at validation time. (`auth/tokens.rs:96-109`)
- Tokens can be **scoped** to specific models or categories, enforcing least-privilege. (`auth/tokens.rs:77-83`)
- **Admin-only middleware** gates sensitive endpoints (IdP config, model management, user admin). (`auth/mod.rs:239-258`)
- **Break-glass bootstrap login** disabled by default (`BREAK_GLASS=false`), requires `BREAK_GLASS=true` + `BOOTSTRAP_USER` + `BOOTSTRAP_PASSWORD` to enable. When active, a one-shot login form appears at `/portal/` — credentials are submitted once via `POST /auth/bootstrap-login` and a session cookie is issued. HTTP Basic auth is not accepted anywhere in the cookie-protected route tree. Only intended for initial setup.

### 2.6 Session & Cookie Security

**Threat:** Session hijacking, XSS-based token theft, CSRF.

**Mitigations:**
- Session cookies set with **HttpOnly** (no JS access), **SameSite=Lax** (CSRF protection), **Max-Age=86400** (24h expiry). (`auth/oidc.rs:301-305`)
- Session tokens are **SHA-256 hashed** before DB storage — even with DB access, raw tokens are unrecoverable. (`auth/sessions.rs:16-20`)
- Cookie cleared on logout with `Max-Age=0`. (`auth/oidc.rs:414-424`)

### 2.7 Frontend Security

**Threat:** XSS, credential exposure, supply chain attacks via dependencies.

**Mitigations:**
- **No `dangerouslySetInnerHTML`** or `innerHTML` usage anywhere in the React codebase. All user content rendered through React's default safe string interpolation.
- **Source maps disabled** in production builds. (`vite.config.ts:16`)
- **No secrets in client code** — no `VITE_` environment variables used. All API endpoints are relative paths. Configuration comes from the backend.
- **No tokens in localStorage/sessionStorage** — authentication relies on HttpOnly cookies only. localStorage stores only the theme preference.
- **Minimal dependencies:** React 19, react-router-dom 7, Recharts, Vite 6. No unnecessary HTTP clients or crypto libraries — uses native `fetch`.
- **ESLint** with TypeScript + React Hooks rules enforced. TypeScript strict mode enabled.
- **Centralised API client** (`api.ts`) with single `request()` wrapper for consistent auth and error handling.
- `encodeURIComponent()` used for URL parameters. (`api.ts`)

### 2.8 Database Security

**Threat:** SQL injection, data corruption, credential exposure.

**Mitigations:**
- **SQLite with WAL mode** for crash recovery and concurrent read integrity. (`db/mod.rs:19, 35`)
- **All queries parameterised** via sqlx — no string concatenation of SQL. Compile-time verification ensures query validity.
- Proper **indexes** on frequently queried columns (token hashes, session hashes). (`migrations/20260212000000_initial.sql:84-91`)
- **Foreign key constraints** enforce referential integrity.
- SQLite database files excluded from git (`.gitignore`: `*.db`, `*.db-shm`, `*.db-wal`).

### 2.9 Docker Build Security

**Threat:** Bloated images, leaked secrets, supply chain attacks.

**Mitigations:**
- **Multi-stage build** (3 stages: ui-builder, rust-builder, runtime). Build tools and source code are not in the final image. (`Dockerfile`)
- **Debian slim runtime** with only `ca-certificates`, `curl`, `libssl3` installed. Package cache removed.
- **Image versions pinned** (`node:22-alpine`, `rust:1.92-bookworm`, `debian:bookworm-slim`) — no `:latest` tags.
- `.dockerignore` excludes `.env`, `target/`, `node_modules/`, documentation, and git history.
- `.env` file excluded from git via `.gitignore`. Secrets passed via environment variables at runtime.

### 2.10 Fair-Use Scheduling & DoS Resistance

**Threat:** Single user monopolising inference resources; API abuse.

**Mitigations:**
- **Fair-queue scheduler** with priority-based weighted queuing. (`scheduler/fairness.rs`)
- **Per-model concurrency gates** limit simultaneous inference requests. (`scheduler/gate.rs`)
- **Configurable queue timeout** with proper `429 Too Many Requests` response and `Retry-After` header. (`api/openai.rs:118-151`)

### 2.11 Secret Management

**Threat:** Credential leakage in logs, code, or Docker images.

**Mitigations:**
- **Structured logging** via `tracing` — token names logged, never token values. User IDs logged, not passwords. (`api/user.rs:91`, `auth/oidc.rs:282`)
- **RUST_LOG** controls verbosity — no debug dumps in production.
- **HuggingFace tokens** (`HF_TOKEN`) accepted via environment variable, not hardcoded.
- **Container API keys** generated as random UUIDs per container, persisted in DB for recovery. (`api/admin.rs:870, 922-934`)
- **OIDC client secrets** stored in DB (see Gap 3.1 for encryption status).

### 2.12 Error Handling — Information Minimisation

**Threat:** Stack traces or internal details leaking to attackers.

**Mitigations:**
- OIDC errors return generic messages: "OIDC configuration error" instead of IdP-specific details. (`auth/oidc.rs:98-100`)
- Backend failures return "Backend unavailable", not container details. (`proxy/streaming.rs:43-51`)
- Invalid tokens return "Invalid token", not "User not found". (`auth/tokens.rs:70`)

---

## 3. Identified Gaps & Recommendations

### 3.1 ~~OIDC Client Secret Stored Plaintext~~ — REMEDIATED

**Status:** Fixed in security hardening pass (2026-02-13).
**Fix:** AES-256-GCM encryption via `db/crypto.rs`. Key derived from `DB_ENCRYPTION_KEY` env var via HKDF-SHA256 (with fixed application-specific salt and info). Existing plaintext secrets and legacy SHA-256-derived ciphertexts are automatically migrated to HKDF-derived encryption on startup. Encryption/decryption integrated into IdP CRUD (`api/admin.rs`) and OIDC client construction (`auth/oidc.rs`). Warns at startup if `DB_ENCRYPTION_KEY` is not set.

### 3.2 ~~Database Error Strings Leaked to Clients~~ — REMEDIATED

**Status:** Fixed in security hardening pass (2026-02-13).
**Fix:** Centralised error helpers in `api/error.rs` (`internal_error`, `api_error`). All admin and user API handlers now return generic "Internal server error" to clients while logging the real error with structured `tracing::error!` fields. Intentional user-facing messages (404 "not found", 400 validation) are preserved.

### 3.3 ~~CORS Permissive~~ — REMEDIATED (updated 2026-04-29)

**Status:** Initially fixed 2026-02-13; updated in 1.8.0 to address gh#2 (CORS allow-list blocked browser clients on origins other than the API hostname).

**Fix (1.8.0):** The single `build_cors_layer` has been replaced by two CORS layers applied at sub-router level:

- **Strict layer** (`/auth/*`, `/api/*`, `/portal/*`, WebUI fallback): `AllowOrigin::list([api_external_url, chat_external_url])`, `allow_credentials(true)`. Preserves the existing two-origin allow-list for all cookie-bearing routes.
- **Bearer layer** (`/v1/*` — OpenAI + Anthropic compat routes): `AllowOrigin::any()`, `allow_credentials(false)`. Safe to open to any origin because `/v1/*` is bearer-only. Browsers do not auto-attach `Authorization: Bearer` or `x-api-key`, so CSRF via a third-party page is impossible. With `allow_credentials(false)`, the session cookie is not sent on cross-origin requests to `/v1/*` either.

**tower-http 0.6 quirk:** When `allow_credentials(true)` is set alongside `AllowOrigin::list([...])`, tower-http emits `Access-Control-Allow-Credentials: true` on every preflight response, including ones from non-matching origins. This is safe: browsers require a valid `Access-Control-Allow-Origin` header before they act on `Allow-Credentials`, and that header is absent for non-matching origins. The spurious `Allow-Credentials: true` on no-match preflights does not expand CORS access.

### 3.4 ~~No `Secure` Flag on Cookies~~ — REMEDIATED

**Status:** Fixed in security hardening pass (2026-02-13).
**Fix:** Centralised cookie construction via `sessions::build_cookie()` / `sessions::clear_cookie()`. `Secure` flag conditionally added based on `SECURE_COOKIES` env var (default `true`). All cookie-setting code paths updated: OIDC callback, bootstrap login (`POST /auth/bootstrap-login`), logout. (`auth/sessions.rs`, `auth/oidc.rs`)

### 3.5 ~~No Security Headers (HSTS, CSP, X-Frame-Options)~~ — REMEDIATED

**Status:** Fixed in security hardening pass (2026-02-13).
**Fix:** Added `security_headers` middleware in `main.rs` that sets all recommended headers: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Strict-Transport-Security: max-age=31536000; includeSubDomains`, `Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'`. HSTS is harmless on HTTP (browsers ignore it).

### 3.6 No HTTP-to-HTTPS Redirect — ACCEPTED RISK

**Status:** Accepted risk (2026-02-13).
**Rationale:** Port 80 is not forwarded in production Docker deployments — only port 443 is exposed. HSTS header is now set (see 3.5), so browsers that have visited once will auto-upgrade. Adding a port 80 listener would increase attack surface for minimal benefit in this deployment model. If the deployment model changes to expose port 80, revisit this item.

### 3.7 ~~API Token Expiration Optional~~ — REMEDIATED

**Status:** Fixed in security hardening pass (2026-02-13).
**Fix:** `tokens::create_token()` now accepts `expires_in_days` parameter, defaulting to 90 days. `expires_at` is set directly in the INSERT statement. API request field changed from `expires_at` (string) to `expires_in_days` (integer). Internal tokens (Open WebUI) remain non-expiring by design. (`auth/tokens.rs`, `api/user.rs`)

### 3.8 No Rate Limiting on Auth Endpoints — PARTIALLY REMEDIATED (2026-04-29)

**Status:** Accepted risk for OIDC paths; `POST /auth/bootstrap-login` now rate-limited.
**Rationale:** OIDC authentication happens at the IdP, not at our endpoints — brute-force protection is the IdP's responsibility. The `/auth/callback` endpoint receives a one-time authorization code that can only be exchanged once. `POST /auth/bootstrap-login` (the break-glass login endpoint introduced in 1.8.0) is rate-limited to 5 attempts per minute per IP via an in-memory `DashMap`. The bootstrap endpoint is still setup-only (`BREAK_GLASS=true`) and disabled by default.

### 3.9 Docker Socket Access — DOCUMENTED (Architectural)

**Status:** Documented trust boundary (2026-02-13).
**Trust model:** The proxy is the single trusted component with Docker API access. Host-level Docker socket access is equivalent to root — a compromised proxy could mount `/` as `/host` and read all files. No mitigation exists within the Docker model itself; this is the fundamental trust boundary. Defence-in-depth options: rootless Docker, Tecnativa/docker-socket-proxy (limits API surface to container CRUD), SELinux/AppArmor profiles on the host. The proxy itself is written in Rust (memory-safe), uses parameterised queries (no injection), and validates all inputs before Docker API calls.

### 3.10 ~~Session/Auth State Cleanup Not Wired~~ — REMEDIATED

**Status:** Fixed in security hardening pass (2026-02-13).
**Fix:** Hourly `tokio::spawn` task in `main.rs` calls `sessions::cleanup_expired()` and deletes expired `oidc_auth_state` rows. `#[allow(dead_code)]` removed from the function.

### 3.11 First OIDC User Auto-Promoted to Admin — DOCUMENTED

**Status:** Documented (2026-02-13).
**Deployment sequence:** (1) Set `BREAK_GLASS=true`, `BOOTSTRAP_USER`, and `BOOTSTRAP_PASSWORD`. (2) Open `/portal/` in a browser, enter credentials in the break-glass login form, receive a session cookie. (3) Configure IdP via the admin UI. (4) Complete the first OIDC login — that user is auto-promoted to admin. (5) Set `BREAK_GLASS=false` in production. The auto-promotion is intentional and matches the single-operator deployment model. Multi-operator deployments should use explicit admin grants via the admin API after initial setup.

### 3.12 ~~No Audit Trail for Admin Actions~~ — REMEDIATED

**Status:** Fixed in security hardening pass (2026-02-13).
**Fix:** All admin mutation handlers in `api/admin.rs` and user token operations in `api/user.rs` now emit structured audit logs via `tracing::info!(target: "audit", action = "...", actor = %session.user_id, resource = %id, ...)`. The `target: "audit"` enables filtering via `RUST_LOG=audit=info` for dedicated audit capture to a file or log aggregator. Covered actions: IdP create/update/disable, category create/update/delete, model register/update/delete, user update, container start/stop, settings update, token create/revoke.

### 3.13 ~~No Input Length Validation~~ — REMEDIATED

**Status:** Fixed (2026-02-14).
**Fix:** Centralised `validate_len` helper in `api/error.rs` with constants: `MAX_NAME` (256), `MAX_URL` (2048), `MAX_DESCRIPTION` (4096), `MAX_SECRET` (4096). Applied to all input-accepting handlers: IdP create/update, category create/update, model register, token create, HF download. Returns 400 with field name and limit on violation.

### 3.14 Container Secrets in `docker inspect` — LOW

**Issue:** Container API keys are passed as environment variables, visible via `docker inspect`.
**Impact:** Anyone with Docker API access (which is limited to the proxy in this architecture) can read container secrets.
**Recommendation:** Consider Docker secrets or file-based injection for defence in depth. Low priority given the proxy already controls the Docker socket.

### 3.15 ~~Health Checks Disabled~~ — REMEDIATED

**Status:** Fixed (2026-02-14).
**Fix:** Enabled health check in `docker-compose.yml`: `curl -sf http://localhost:3000/auth/providers || exit 1`, 30s interval, 5s timeout, 3 retries, 15s start period. Uses `/auth/providers` (lightweight unauthenticated JSON endpoint) to verify the proxy is actually running.

---

## 4. Dependency Summary

| Component | Library | Security Notes |
|-----------|---------|----------------|
| TLS | rustls 0.23 | Pure Rust, audited, no C memory risks |
| HTTP | axum 0.8 / hyper | Rust async, well-maintained |
| Docker API | bollard | Safe Rust bindings, no shell exec |
| Database | sqlx (SQLite) | Compile-time query verification |
| OIDC | openidconnect | Handles protocol correctly, PKCE support |
| HTTP client | reqwest | Redirect-disabled for OIDC flows |
| Frontend | React 19, Vite 6 | Current, no known CVEs |
| Linting | ESLint + TypeScript strict | Catches common errors at build time |

---

## 5. Summary Matrix

| Category | Status | Notes |
|----------|--------|-------|
| Network isolation | **Strong** | Internal-only Docker network, no port bindings |
| Container privilege | **Strong** | Unique UIDs, read-only mounts, no root |
| Authentication | **Strong** | OIDC+PKCE, SHA-256 hashed tokens, 90-day default expiry |
| Memory safety | **Strong** | Rust throughout, rustls for TLS |
| Transport security | **Strong** | TLS with ACME, HSTS header set |
| Cookie security | **Strong** | HttpOnly + SameSite + conditional Secure flag |
| SQL injection | **Strong** | Parameterised queries with compile-time checks |
| XSS prevention | **Strong** | No innerHTML, no dangerouslySetInnerHTML |
| Error handling | **Strong** | Generic errors to clients, structured server-side logging |
| CORS | **Strong** | Two-layer split: strict allow-list for cookie routes, open to any origin for bearer-only `/v1/*` (CSRF-immune) |
| Secret storage | **Strong** | AES-256-GCM encrypted client secrets at rest |
| Security headers | **Strong** | HSTS, CSP, X-Frame-Options, X-Content-Type-Options |
| Audit trail | **Good** | Structured audit logging on all admin mutations |
| Rate limiting | **Partial** | Fair-queue for inference; auth rate limiting accepted risk (IdP handles it) |

---

## 6. Remaining Items

| # | Item | Priority | Status |
|---|------|----------|--------|
| 3.14 | Container secrets in `docker inspect` | Low | Open |

---

## 7. Remediation Log

| Date | Items | Summary |
|------|-------|---------|
| 2026-02-13 | 3.1, 3.2, 3.3, 3.4, 3.5, 3.7, 3.10, 3.12 | Security hardening pass: AES-256-GCM encryption for IdP secrets, generic error responses, explicit CORS, Secure cookies, security headers, 90-day token expiry, session cleanup, audit logging |
| 2026-02-13 | 3.6, 3.8 | Accepted risk: HTTP→HTTPS redirect (port 80 not exposed), auth rate limiting (IdP handles brute-force) |
| 2026-02-13 | 3.9, 3.11 | Documented: Docker socket trust boundary, first-user admin promotion sequence |
| 2026-02-14 | 3.13, 3.15 | Input length validation on all handlers, Docker health checks enabled |
| 2026-04-29 | 3.3, 3.8 | CORS two-layer split: bearer-only `/v1/*` open to any origin (CSRF-immune), strict allow-list retained for cookie routes. `POST /auth/bootstrap-login` rate-limited (5/min/IP). HTTP Basic auth removed from all middleware. |
