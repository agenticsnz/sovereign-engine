# Sovereign Engine — Architecture

## System Overview

Single Docker image containing:
- **Rust reverse proxy** (axum 0.8) — authentication, scheduling, API gateway
- **React SPA** — admin dashboard, token management, usage analytics, reservation calendar
- **SQLite database** — WAL mode, embedded migrations
- Manages **llama.cpp backend containers** (NVIDIA CUDA, AMD ROCm, or CPU-only) via Docker API (bollard 0.18)

## Component Diagram

```
┌─────────────────────────────────────────────────────────┐
│                    Docker Host                          │
│                                                         │
│  ┌─── sovereign-public (bridge) ──────────────────┐     │
│  │                                                │     │
│  │  ┌──────────────────────────────────────────┐  │     │
│  │  │        Sovereign Engine Container         │  │     │
│  │  │                                          │  │     │
│  │  │  ┌────────────────────┐                  │  │     │
│  │  │  │   Rust Proxy       │                  │  │     │
│  │  │  │   (axum)           │──── /config ─────│──│── SQLite DB
│  │  │  │   :3000 / :443     │                  │  │     │
│  │  │  └──────┬─────────────┘                  │  │     │
│  │  │         │  serves                        │  │     │
│  │  │  ┌──────┴─────────────┐                  │  │     │
│  │  │  │ api.* → Portal/API │                  │  │     │
│  │  │  │ chat.* → WebUI     │                  │  │     │
│  │  │  └────────────────────┘                  │  │     │
│  │  └──────────────┬───────────────────────────┘  │     │
│  │                 │                              │     │
│  └─────────────────┼──────────────────────────────┘     │
│                    │                                     │
│  ┌─── sovereign-internal (bridge, internal: true) ─┐    │
│  │                 │                               │    │
│  │     ┌───────────┴────────────┐                  │    │
│  │     │     Docker API         │                  │    │
│  │     └───┬───────────┬───────┘                  │    │
│  │         │           │                           │    │
│  │  ┌──────┴──┐  ┌─────┴───┐                      │    │
│  │  │ llama   │  │ llama   │  ...                  │    │
│  │  │ :8080   │  │ :8080   │                       │    │
│  │  │ Model A │  │ Model B │                       │    │
│  │  └─────────┘  └─────────┘                       │    │
│  │                                                 │    │
│  └─────────────────────────────────────────────────┘    │
│                                                         │
│  Volumes: /config (SQLite DB), /models (shared),        │
│           /var/run/docker.sock (Docker API)              │
└─────────────────────────────────────────────────────────┘
```

## Network Topology

**sovereign-public** (bridge network):
- Host-facing network. The proxy container exposes ports 3000 (dev) or 443 (production) to the host.
- Standard Docker bridge network.

**sovereign-internal** (bridge, `internal: true`):
- Isolated network for proxy <-> backend communication only.
- `internal: true` means no external/host access — backend containers cannot reach the internet or the host directly.
- The proxy container is attached to both networks.
- Backend containers are attached only to `sovereign-internal`.
- Proxy reaches backends by container name: `http://sovereign-llamacpp-{model_id}:8080`
- No host port bindings on backend containers.

**Per-container UID isolation:**
- Each backend container runs as a unique non-root user.
- UID allocated randomly in 10000–65000 with collision avoidance (checks running containers before assigning).
- Prevents one backend container from accessing another's process space.

## Auth Flow

### Bootstrap path (break-glass)

```
Client -> Basic Auth header -> session_auth_middleware / /auth/me -> validate_bootstrap()
  +-- BREAK_GLASS=true + credentials match: silently creates session → SessionAuth { is_admin: true }
```

### OIDC path

```
1. GET /auth/providers -> list enabled IdPs
2. GET /auth/login?idp=<id> -> generate CSRF token + nonce + PKCE verifier, store in oidc_auth_state table
   -> 302 redirect to IdP authorization endpoint
3. IdP redirects back: GET /auth/callback?code=<code>&state=<state>
   -> Validate CSRF state against oidc_auth_state table
   -> Exchange code for tokens with IdP (PKCE verified)
   -> Verify ID token (nonce check)
   -> Create/update user in users table
   -> Create session in sessions table (SHA-256 hashed token)
   -> Set cookie: se_session=<token>; HttpOnly; Path=/; Max-Age=86400
   -> 302 redirect to /
4. Subsequent requests: session cookie -> session_auth_middleware -> SessionAuth in extensions
```

### Bearer token path (API)

```
Client -> Authorization: Bearer se-<uuid> -> bearer_auth_middleware
  -> SHA-256 hash token -> lookup in tokens table by token_hash
  -> Extract: user_id, token_id, category_id, specific_model_id, is_admin
  -> AuthUser in request extensions
```

### Middleware stack

Routing is split by subdomain (see [ADR 026](decisions/026-subdomain-routing.md)):

```
Host-based dispatch (Host header → router selection)
├── api.<domain>  (API router)
│   ├── /auth/*          → No auth (public routes for OIDC flow)
│   ├── /api/*           → session_auth_middleware (cookie or Basic auth)
│   │   └── /api/admin/* → + admin_only_middleware (checks SessionAuth.is_admin)
│   ├── /v1/*            → bearer_auth_middleware (API token)
│   └── /portal/*        → Static file serving (React SPA)
├── chat.<domain> (Chat router)
│   └── /*               → session_auth_redirect_middleware → Open WebUI reverse proxy
└── other                → 421 Misdirected Request

Global layers: security_headers, TraceLayer, CompressionLayer, CorsLayer
```

When `API_HOSTNAME` == `CHAT_HOSTNAME` (dev mode), all routes are combined on a single host with Open WebUI as the fallback.

## Open WebUI Routing

Open WebUI cannot run on a subpath — it assumes it owns `/`. The proxy uses subdomain-based routing:

- **`api.<domain>/portal/*`** — Served as static files (React SPA with `base: '/portal/'`). Falls back to `index.html` for SPA routing.
- **`chat.<domain>/*`** — Reverse-proxied to Open WebUI (`WEBUI_BACKEND_URL`, default `http://open-webui:8080`). Requires session auth; unauthenticated browsers are redirected to the API subdomain's portal.

Session cookies are shared across subdomains via `COOKIE_DOMAIN` (e.g. `Domain=.example.com`). The Open WebUI proxy injects trusted-header SSO so users authenticated via Sovereign Engine's OIDC are automatically logged in to Open WebUI.

## Request Lifecycle (OpenAI API)

```
1. POST /v1/chat/completions
   |
2. bearer_auth_middleware
   |  Validate token -> AuthUser { user_id, category_id, specific_model_id }
   |
3. chat_completions handler
   |  Parse request body (extract model name, stream flag)
   |
4. proxy_completion()
   |
5. scheduler.resolve_model(model_name, category_id, specific_model_id)
   |  Resolution chain:
   |  a) specific_model_id from token -> direct lookup
   |  b) category_id from token -> preferred model -> any loaded model in category
   |  c) model_name as model ID or hf_repo -> direct lookup
   |  d) model_name as category name -> resolve via category
   |  e) Error: model not found
   |
6. Check model.loaded -> 503 if not loaded
   |
7. Concurrency gate: acquire slot (or queue with fair-use priority)
   |
8. Build backend URL: http://sovereign-llamacpp-{model_id}:8080/v1/chat/completions
   |
9. proxy_to_backend(client, url, body, is_streaming)
   |  Forward request body to backend, stream response back
   |
10. Fire-and-forget usage logging (tokio::spawn)
    |  Log to usage_log: token_id, user_id, model_id, category_id, latency_ms
    |
11. Return response to client (streaming SSE or JSON)
```

## Database Schema

Eleven migration files define the schema:

| Migration | Purpose |
|---|---|
| `20260212000000_initial.sql` | Core tables: `idp_configs`, `model_categories`, `models`, `users`, `tokens`, `usage_log`, `idp_model_access`, `sessions`, `oidc_auth_state` + indexes |
| `20260212000001_pkce_verifier.sql` | Adds `pkce_verifier` column to `oidc_auth_state` for PKCE-required OIDC flows |
| `20260212000002_container_secrets.sql` | Creates `container_secrets` table: per-container UID allocation and API key |
| `20260213000000_internal_tokens.sql` | Adds `internal` flag to `tokens` (auto-provisioned, e.g. Open WebUI), `model_metadata` to `models` |
| `20260213000001_context_length.sql` | Adds `context_length` column to `models` |
| `20260213000002_remove_vllm.sql` | Data migration: converts vLLM backend models to llama.cpp |
| `20260213000003_gguf_metadata.sql` | Adds GGUF metadata columns to `models`: `n_layers`, `n_heads`, `n_kv_heads`, `embedding_length` |
| `20260213100000_parallel_slots.sql` | Adds `parallel_slots` column to `container_secrets` |
| `20260213100001_settings.sql` | Creates `settings` table (key-value); seeds fairness tuning defaults |
| `20260215000000_reservations.sql` | Creates `reservations` table: time-slot reservations with status machine, admin approval |
| `20260215000001_meta_tokens.sql` | Adds `meta` flag to `tokens` (bookkeeping tokens for per-user Open WebUI attribution) |

### Entity Relationships

```
idp_configs 1──∞ users (idp_id)
users 1──∞ tokens (user_id)
users 1──∞ sessions (user_id)
users 1──∞ usage_log (user_id)
users 1──∞ reservations (user_id)
tokens 1──∞ usage_log (token_id)
model_categories 1──∞ models (category_id)
model_categories 1──? models (preferred_model_id)
model_categories 1──∞ tokens (category_id)
model_categories 1──∞ idp_model_access (category_id)
idp_configs 1──∞ idp_model_access (idp_id)
models 1──? container_secrets (model_id)
settings — standalone key-value table
```

## Module Map

```
proxy/src/
├── main.rs              — Entry point. Loads config, initialises DB/Docker/Scheduler,
│                          computes CSP hashes from index.html, builds host-dispatched
│                          router with middleware layers, starts HTTP or HTTPS server.
├── config.rs            — AppConfig struct. Loads all settings from environment variables.
│                          Provides helpers: has_bootstrap_creds(), validate_bootstrap_creds(),
│                          tls_paths(), acme_config(), api_external_url(), chat_external_url().
├── tls.rs               — TLS server setup using rustls + axum-server. ACME TLS-ALPN-01
│                          with multi-domain SAN support.
├── metrics.rs           — MetricsBroadcaster: collects GPU memory, CPU, disk, queue, container
│                          stats every 2 s and broadcasts via tokio::broadcast for SSE consumers.
│
├── api/
│   ├── mod.rs           — API route tree. Nests /admin (with admin_only middleware) and /user.
│   ├── admin.rs         — Admin endpoints: CRUD for IdPs, categories, models, users.
│   │                      Container start/stop. System status. Settings GET/PUT.
│   ├── user.rs          — User endpoints: token list/mint/revoke, usage statistics,
│   │                      categories/models read, disk usage, unified SSE event stream.
│   ├── openai.rs        — OpenAI-compatible /v1/chat/completions, /v1/completions, /v1/models.
│   │                      Contains proxy_completion() — the core request lifecycle function.
│   ├── hf.rs            — HuggingFace integration: search models, background download with
│   │                      progress tracking, disk usage monitoring, auto-registration on completion.
│   ├── reservation.rs   — Reservation user + admin routes: create, cancel, approve, reject,
│   │                      force activate/deactivate, calendar, container start/stop during reservation.
│   └── error.rs         — Shared error helpers: internal_error(), validate_len().
│
├── auth/
│   ├── mod.rs           — Auth types (AuthUser, SessionAuth). Three middleware functions:
│   │                      bearer_auth_middleware, session_auth_middleware,
│   │                      session_auth_redirect_middleware, admin_only_middleware.
│   ├── bootstrap.rs     — Bootstrap basic auth validation (break-glass). Silently creates a
│   │                      session on /auth/me so the portal SPA has a cookie.
│   ├── oidc.rs          — OIDC routes: /auth/providers, /auth/login, /auth/callback,
│   │                      /auth/logout, /auth/me. Handles OIDC discovery, auth URL generation,
│   │                      code exchange (with PKCE), user creation, session creation.
│   ├── sessions.rs      — Session CRUD: create_session, validate_session, delete_session.
│   │                      SHA-256 hashed tokens, 24h TTL, cookie name: se_session.
│   └── tokens.rs        — API token validation: hash incoming token, lookup by token_hash,
│                          check expiry/revocation, return AuthUser. Also handles internal
│                          token provisioning for Open WebUI.
│
├── db/
│   ├── mod.rs           — Database struct wrapping sqlx Pool<Sqlite>. Connection with WAL mode,
│   │                      5 max connections, 5s busy timeout. migrate!() macro for compile-time migrations.
│   ├── models.rs        — Shared DB query helpers and sqlx::FromRow structs.
│   └── crypto.rs        — AES-256-GCM encryption/decryption for IdP client secrets at rest.
│                          Key derived via SHA-256 from DB_ENCRYPTION_KEY env var.
│
├── docker/
│   ├── mod.rs           — DockerManager: connects to Docker, ensures sovereign-internal network exists,
│   │                      lists managed containers by label (managed-by=sovereign-engine).
│   │                      allocate_uid(): random UID in 10000–65000 with collision avoidance.
│   │                      Dispatches start/stop to llama.cpp backend.
│   └── llamacpp.rs      — LlamacppConfig struct (includes optional mmproj_path for multimodal
│                          projectors). start_llamacpp(): creates container (CUDA, ROCm, or
│                          CPU-only), bind mount for /models (read-only), internal network
│                          attachment, unique UID, labels, per-container API key. Container named
│                          sovereign-llamacpp-{model_id}. Command construction is delegated to a
│                          pure build_llamacpp_cmd() helper which emits --mmproj when a projector
│                          is present and readable (missing files degrade to text-only with a
│                          warning). stop_llamacpp(): stop + remove. check_llamacpp_health():
│                          HTTP /health check.
│
├── proxy/
│   ├── mod.rs           — Proxy module declaration.
│   ├── streaming.rs     — proxy_to_backend(): forwards request to llama.cpp backend.
│   │                      Handles both streaming (SSE) and non-streaming responses.
│   └── webui.rs         — Open WebUI reverse proxy handler. Injects trusted-header SSO,
│                          rewrites cookies, proxies all HTTP methods.
│
└── scheduler/
    ├── mod.rs           — Scheduler struct. Wraps RequestQueue + FairnessSettings + active reservation.
    │                      Delegates resolve_model() to resolver. Exposes queue depth and stats.
    ├── queue.rs         — RequestQueue: per-category priority queues with depth and avg wait tracking.
    ├── fairness.rs      — Priority calculation: base_priority + wait_time_bonus - recent_usage_penalty.
    ├── resolver.rs      — Model resolution chain: specific_model_id -> category_id -> model ID/hf_repo
    │                      -> category name. Uses preferred model, falls back to any loaded model.
    ├── usage.rs         — log_usage(): inserts into usage_log table. Called fire-and-forget from
    │                      openai.rs after proxying each request.
    ├── gate.rs          — Concurrency gate: per-model semaphore limiting parallel inference slots.
    │                      GateSnapshot for metrics. Recovered from container_secrets on restart.
    ├── reservation.rs   — Reservation state machine: tick_reservations() runs every 30s to
    │                      activate approved, complete expired, and cancel stale reservations.
    │                      ReservationBroadcaster for SSE push notifications.
    │                      ActiveReservation in-memory cache with DB persistence + recovery.
    └── settings.rs      — FairnessSettings: runtime-configurable tuning. load_settings() / save_setting()
                           from/to the `settings` DB table.
```

### GPU Memory Reporting

GPU memory stats are collected by `MetricsBroadcaster` and tuned for AMD Strix Halo APU: GTT (system-shared) and VRAM are summed to reflect total available GPU memory, since Strix Halo reports usable memory across both pools.

## Frontend Architecture

**Stack:** React 19, TypeScript 5.7, Vite 6, Recharts

**Served at:** `/portal/*` (Vite `base: '/portal/'`)

**Structure:**
- `App.tsx` — Root component. Manages auth state via `GET /auth/me`. Shows `LoginPage` or `AuthenticatedApp`.
- `LoginPage` — Lists OIDC providers as buttons, plus collapsible bootstrap basic auth form.
- `AuthenticatedApp` — Navigation bar + `<Routes>`. Admin links shown conditionally on `user.is_admin`.
- `api.ts` — Typed fetch wrapper. Central `request<T>()` function handles auth, errors, 401 redirect.
- `types.ts` — TypeScript interfaces matching all API response contracts.

### Pages

| Route | Component | Description |
|---|---|---|
| `/` | Dashboard | Usage charts (pie by model, timeline bar), summary stats |
| `/tokens/mint` | TokenMint | Mint new API token with category/model selection |
| `/tokens` | TokenManage | List, view, revoke tokens |
| `/models` | Models | View loaded models (from /v1/models) |
| `/reservations` | Reservations | View and create GPU reservations, week calendar |
| `/admin/idp` | IdpConfig | CRUD for OIDC identity providers |
| `/admin/models` | ModelMapping | Category/model management, container start/stop |
| `/admin/users` | Users | User list, admin toggle |
| `/admin/system` | System | Disk usage, container health, queue depths, GPU metrics |
| `/admin/reservations` | AdminReservations | Approve/reject/activate/deactivate reservations |

### Charts

Recharts library — `UsagePieChart` (usage by category) and `UsageTimelineChart` (requests over time).

### Common Components

`ConfirmDialog`, `CopyButton`, `ErrorAlert`, `LoadingSpinner`, `WeekCalendar`.

## Architecture Decision Records

| ADR | Decision | Key trade-off |
|-----|----------|---------------|
| [001](decisions/001-llamacpp-over-vllm.md) | llama.cpp over vLLM | Simpler codebase, no vLLM features (PagedAttention) |
| [002](decisions/002-portal-subpath.md) | ~~React SPA at `/portal` subpath~~ | Superseded by ADR 026 |
| [003](decisions/003-model-host-path.md) | MODEL_HOST_PATH for bind mounts | Host vs container path distinction |
| [004](decisions/004-db-encryption-key.md) | Optional AES-256-GCM for IdP secrets | Security vs deployment simplicity |
| [005](decisions/005-bootstrap-auth.md) | Break-glass bootstrap auth | Zero-dependency initial setup |
| [006](decisions/006-strix-halo-gpu-tuning.md) | GPU memory tuned for Strix Halo | GTT+VRAM summed for unified memory |
| [007](decisions/007-csp-hash-extraction.md) | CSP hashes computed at startup | Frontend-proxy build decoupling |
| [008](decisions/008-container-uid-allocation.md) | Random UID allocation | Process isolation, no deterministic collisions |
| [009](decisions/009-unified-sse-events.md) | Unified SSE event stream | Single connection, no WebSocket complexity |
| [010](decisions/010-broadcaster-pattern.md) | tokio::broadcast for fan-out | Lock-free, lagged messages dropped |
| [011](decisions/011-fire-and-forget-usage-logging.md) | Fire-and-forget usage logging | Zero latency impact, best-effort writes |
| [012](decisions/012-session-token-strategy.md) | SHA-256 hashed session tokens | DB leak protection, 24h TTL |
| [013](decisions/013-middleware-composition.md) | Per-route middleware composition | Exact auth per route group |
| [014](decisions/014-model-resolution-chain.md) | Model resolution chain | Token constraints > category > name |
| [015](decisions/015-per-container-api-keys.md) | Per-container API keys | Defence-in-depth for backends |
| [016](decisions/016-logarithmic-fairness.md) | Logarithmic fairness formula | Intuitive scaling, runtime-tunable |
| [017](decisions/017-concurrency-gate-raii.md) | Concurrency gate as RAII guard | Guaranteed slot release on drop |
| [018](decisions/018-reservation-state-machine.md) | Reservation state machine | Auto-transitions, 30s tick delay |
| [019](decisions/019-oidc-callback-url.md) | OIDC callback from API hostname | API subdomain for auth routes |
| [020](decisions/020-anyhow-error-handling.md) | Anyhow for errors, no custom types | Less boilerplate, no compile-time error matching |
| [021](decisions/021-webui-trusted-header-sso.md) | Open WebUI trusted-header SSO | Seamless SSO, relies on network isolation |
| [022](decisions/022-internal-meta-tokens.md) | Internal and meta token flags | Clean user UX, hidden system plumbing |
| [023](decisions/023-sse-connection-delay.md) | SSE connection 2s delay | Prevents HTTP/1.1 connection exhaustion on load |
| [024](decisions/024-huggingface-background-download.md) | HuggingFace background download | No timeout, progress tracking, auto-registration |
| [025](decisions/025-token-soft-delete.md) | Token soft delete | Preserves usage history |
| [026](decisions/026-subdomain-routing.md) | Subdomain-based routing | Host dispatch, cross-subdomain cookies |

### Auth State Management

- On load: `GET /auth/me` -> set `user` state or show login
- `setOnUnauthorized()` callback: on any 401, clear user state -> show login
- Logout: `POST /auth/logout` -> clear user state

### SSE Event Stream

`GET /api/user/events` provides a unified SSE stream merging:
- **`metrics`** events (every 2s) — GPU memory, CPU, disk, queue stats, active reservation
- **`reservations_changed`** events — emitted on any reservation state change

Admins receive the full `MetricsSnapshot`; non-admin users receive only `gpu_memory`, `active_reservation`, and `timestamp`.
