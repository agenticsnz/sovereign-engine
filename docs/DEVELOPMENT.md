# Development Guide

## First-Time Setup

1. **Install Rust 1.84+:**
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source ~/.cargo/env
   ```

2. **Install Node.js 22+:**
   ```bash
   curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash
   source ~/.bashrc  # or ~/.zshrc
   nvm install 22
   ```

3. **Install Docker** (for running llama.cpp backends — not required for proxy-only development):
   Follow the [Docker install guide](https://docs.docker.com/engine/install/) for your platform.

4. **Clone and configure:**
   ```bash
   git clone https://github.com/rauxon/SovereignEngine.git
   cd SovereignEngine
   cp .env.example .env
   ```
   Edit `.env` — at minimum set:
   ```
   LISTEN_ADDR=0.0.0.0:31000
   BOOTSTRAP_USER=admin
   BOOTSTRAP_PASSWORD=changeme
   BREAK_GLASS=true
   SECURE_COOKIES=false
   ```

5. **Build and run the proxy:**
   ```bash
   cd proxy
   cargo run
   ```
   The proxy reads `.env` via `dotenvy::dotenv()`. On first run, SQLite migrations run automatically.

6. **Build and run the UI dev server** (in a separate terminal):
   ```bash
   cd ui
   npm install
   npm run dev
   ```
   Vite proxies `/api`, `/auth`, and `/v1` to `http://localhost:31000` (configured in `vite.config.ts`).

7. **Verify it works:**
   ```bash
   # Health check
   curl -s http://localhost:31000/api/user/health | jq .

   # Open the dashboard
   open http://localhost:5173/portal  # or xdg-open on Linux
   ```
   Log in with **admin** / **changeme** (the bootstrap credentials from `.env`).

---

## Prerequisites

- **Rust 1.84+** — install via [rustup](https://rustup.rs/)
- **Node.js 22+** — install via [nvm](https://github.com/nvm-sh/nvm)
- **Docker** — required for running backend containers (llama.cpp), not required for proxy development

---

## Project Structure

```
SovereignEngine/
├── proxy/                    # Rust reverse proxy (Cargo workspace)
│   ├── Cargo.toml
│   ├── migrations/           # SQLite migrations (sqlx)
│   │   ├── 20260212000000_initial.sql
│   │   ├── 20260212000001_pkce_verifier.sql
│   │   ├── 20260212000002_container_secrets.sql
│   │   ├── 20260213000000_internal_tokens.sql
│   │   ├── 20260213000001_context_length.sql
│   │   ├── 20260213000002_remove_vllm.sql
│   │   ├── 20260213000003_gguf_metadata.sql
│   │   ├── 20260213100000_parallel_slots.sql
│   │   ├── 20260213100001_settings.sql
│   │   ├── 20260215000000_reservations.sql
│   │   └── 20260215000001_meta_tokens.sql
│   └── src/
│       ├── main.rs           # Entry point, router setup, CSP hash extraction, server startup
│       ├── config.rs         # Environment variable configuration
│       ├── tls.rs            # TLS termination (rustls), ACME support
│       ├── metrics.rs        # MetricsBroadcaster (GPU, CPU, disk, queue stats via SSE)
│       ├── api/
│       │   ├── mod.rs        # API route tree (/api/admin/*, /api/user/*)
│       │   ├── admin.rs      # Admin endpoints (IdP, categories, models, users, containers, system, settings)
│       │   ├── user.rs       # User endpoints (tokens, usage, categories, models, disk, SSE events)
│       │   ├── openai.rs     # OpenAI-compatible endpoints (/v1/chat/completions, /v1/models)
│       │   ├── hf.rs         # HuggingFace integration (search, download)
│       │   ├── reservation.rs # Reservation user + admin routes
│       │   └── error.rs      # Shared error helpers
│       ├── auth/
│       │   ├── mod.rs        # Auth types (AuthUser, SessionAuth), middleware (bearer, session, admin)
│       │   ├── bootstrap.rs  # Bootstrap credential validation for /auth/bootstrap-login
│       │   ├── oidc.rs       # OIDC login/callback flow (with PKCE)
│       │   ├── sessions.rs   # Session management (cookie-based)
│       │   └── tokens.rs     # API token validation (SHA-256 hashed)
│       ├── db/
│       │   ├── mod.rs        # Database connection, migration runner
│       │   ├── models.rs     # Shared query helpers, sqlx::FromRow structs
│       │   └── crypto.rs     # AES-256-GCM encryption for IdP secrets at rest
│       ├── docker/
│       │   ├── mod.rs        # Docker manager (bollard), UID allocation, container listing
│       │   └── llamacpp.rs   # llama.cpp container lifecycle (CUDA/ROCm/CPU, start, stop, health)
│       ├── proxy/
│       │   ├── mod.rs        # HTTP proxy module
│       │   ├── streaming.rs  # Streaming proxy to backends
│       │   └── webui.rs      # Open WebUI reverse proxy with trusted-header SSO
│       └── scheduler/
│           ├── mod.rs        # Scheduler struct, queue access
│           ├── queue.rs      # Per-category request queues
│           ├── fairness.rs   # Priority calculation (usage decay + wait-time boost)
│           ├── resolver.rs   # Model resolution chain
│           ├── usage.rs      # Usage logging to SQLite
│           ├── gate.rs       # Per-model concurrency gate (semaphore)
│           ├── reservation.rs # Reservation state machine, tick task, SSE broadcast
│           └── settings.rs   # Runtime-configurable fairness settings
├── ui/                       # React frontend (Vite + TypeScript)
│   ├── package.json
│   ├── vite.config.ts        # Dev proxy config (/api, /auth, /v1 → localhost:31000), base: '/portal/'
│   └── src/
│       ├── main.tsx          # React entry point
│       ├── App.tsx           # Root component, routing, auth state, login page
│       ├── api.ts            # Typed fetch wrapper for all API endpoints
│       ├── types.ts          # TypeScript interfaces matching API contracts
│       ├── components/
│       │   ├── charts/       # Recharts components (UsagePieChart, UsageTimelineChart)
│       │   └── common/       # Shared components (ConfirmDialog, CopyButton, ErrorAlert, LoadingSpinner, WeekCalendar)
│       └── pages/
│           ├── user/         # Dashboard, TokenMint, TokenManage, Models, Reservations
│           └── admin/        # IdpConfig, ModelMapping, Users, System, AdminReservations
├── Dockerfile                # Multi-stage build (Node → Rust → Debian slim runtime)
├── docker-compose.yml        # Dev compose (HTTP on port 3000)
├── docker-compose.nvidia.yml # NVIDIA GPU overlay
├── docker-compose.rocm.yml   # AMD ROCm GPU overlay
├── scripts/
│   └── generate-dev-certs.sh # Self-signed TLS cert generator
└── .env.example              # All env vars with descriptions
```

---

## Building

### Rust Proxy

```bash
cd proxy
cargo build           # Debug build
cargo build --release # Production build
```

### React UI

```bash
cd ui
npm install
npm run build         # Production build to ui/dist/
npm run dev           # Vite dev server with hot reload (port 5173)
```

### Docker Image

```bash
docker build -t sovereign-engine .
```

---

## Running Locally

### Proxy (Rust)

```bash
cp .env.example .env
# Edit .env — at minimum set LISTEN_ADDR=0.0.0.0:31000
cd proxy
cargo run
```

The proxy reads `.env` via `dotenvy::dotenv()`.

### UI (Vite Dev Server)

```bash
cd ui
npm run dev
```

Vite proxies `/api`, `/auth`, and `/v1` to `http://localhost:31000` (configured in `vite.config.ts`).

---

## Adding API Endpoints

Pattern for a new endpoint:

1. **Define the route** in the appropriate `routes()` function (e.g., `api/admin.rs`).
2. **Create the handler function:**
   ```rust
   async fn handler(
       State(state): State<Arc<AppState>>,
       Extension(auth): Extension<SessionAuth>,
       Json(body): Json<RequestType>,
   ) -> impl IntoResponse {
       // ...
   }
   ```
3. **Extract auth context:** `Extension(auth)` — use `AuthUser` for bearer token routes, `SessionAuth` for session/cookie routes.
4. **Run DB queries:**
   ```rust
   sqlx::query_as::<_, (String, i64)>("SELECT name, count FROM table WHERE id = ?")
       .bind(&id)
       .fetch_optional(&state.db.pool)
       .await
   ```
5. **Return a response:**
   ```rust
   // Success
   Json(serde_json::json!({ "id": id, "name": name })).into_response()
   // Error
   (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Not found" }))).into_response()
   ```

### Middleware Stack

Applied in `main.rs`:

| Route | Middleware | Auth type |
|---|---|---|
| `/auth/*` | None | Unauthenticated (login/callback) |
| `/api/*` | `session_auth_middleware` | Cookie session |
| `/api/admin/*` | `session_auth_middleware` + `admin_only_middleware` | Admin session only |
| `/v1/*` | `bearer_auth_middleware` | API token (Bearer header) |
| `/portal/*` | None | Static file serving |
| `/*` (fallback) | `session_auth_redirect_middleware` | Session required, redirect for browsers |

---

## Adding UI Pages

1. Create the component in `ui/src/pages/{user,admin}/YourPage.tsx`.
2. Add an API function in `ui/src/api.ts` using the `request<T>()` wrapper.
3. Define TypeScript interfaces in `ui/src/types.ts`.
4. Register the route in `ui/src/App.tsx` inside `AuthenticatedApp`'s `<Routes>`.
5. Add a nav link in the `<nav>` bar.

### The `request<T>()` Function

Defined in `api.ts`, it handles:

- Setting `Content-Type: application/json`
- 401 responses — calls the `onUnauthorized` callback (redirects to login)
- Error extraction from response body
- 204 No Content — returns `undefined` instead of parsing JSON

---

## Database Migrations

Migrations live in `proxy/migrations/`, named by convention: `YYYYMMDDHHMMSS_description.sql`.

The `sqlx::migrate!("./migrations")` macro embeds migrations at compile time. To add a new migration:

1. Create `proxy/migrations/YYYYMMDDHHMMSS_description.sql`.
2. Write the SQL.
3. Rebuild — the migration runs automatically on startup via `db.migrate()`.

### Current Migrations

| File | Purpose |
|---|---|
| `20260212000000_initial.sql` | Core tables: `idp_configs`, `model_categories`, `models`, `users`, `tokens`, `usage_log`, `idp_model_access`, `sessions`, `oidc_auth_state` + indexes |
| `20260212000001_pkce_verifier.sql` | Adds `pkce_verifier` to `oidc_auth_state` for PKCE-required OIDC flows |
| `20260212000002_container_secrets.sql` | Creates `container_secrets` table (per-container UID + API key) |
| `20260213000000_internal_tokens.sql` | Adds `internal` flag to `tokens`, `model_metadata` to `models` |
| `20260213000001_context_length.sql` | Adds `context_length` to `models` |
| `20260213000002_remove_vllm.sql` | Data migration: converts vLLM models to llama.cpp |
| `20260213000003_gguf_metadata.sql` | Adds GGUF metadata columns (`n_layers`, `n_heads`, `n_kv_heads`, `embedding_length`) |
| `20260213100000_parallel_slots.sql` | Adds `parallel_slots` to `container_secrets` |
| `20260213100001_settings.sql` | Creates `settings` key-value table with fairness defaults |
| `20260215000000_reservations.sql` | Creates `reservations` table (time-slot booking with status machine) |
| `20260215000001_meta_tokens.sql` | Adds `meta` flag to `tokens` for Open WebUI attribution |

---

## Testing

### Rust

```bash
cd proxy

# Run all tests
cargo test

# Run a specific test module
cargo test reservation_tests

# Lint
cargo clippy -- -D warnings

# Format check
cargo fmt --check
```

### React UI

```bash
cd ui

# Lint
npm run lint

# Type check
npm run typecheck
```

### Integration / Smoke Tests

```bash
# Reservation API smoke test
tests/test_reservations.sh

# Queue demo (fair-use scheduler exercise)
cargo run --example queue_demo
```

---

## Key Crate Reference

| Crate | Version | Purpose |
|---|---|---|
| axum | 0.8 | Web framework (handlers, routing, middleware, extractors) |
| bollard | 0.18 | Docker API client (container lifecycle, network management) |
| openidconnect | 4 | OIDC client (discovery, auth URL, token exchange, PKCE) |
| sqlx | 0.8 | SQLite async queries, compile-time migrations |
| reqwest | 0.12 | HTTP client (backend proxy, HuggingFace API) |
| tower-http | 0.6 | HTTP middleware (CORS, compression, tracing, static files) |
| rustls | 0.23 | TLS termination |
| serde / serde_json | 1 | Serialization / JSON |
| tokio | 1 | Async runtime |
| tracing | 0.1 | Structured logging |
| sha2 | 0.10 | SHA-256 hashing (tokens, sessions, CSP) |
| base64 | 0.22 | Base64 encoding (CSP hashes) |
| chrono | 0.4 | Date/time handling (reservations, expiry) |
