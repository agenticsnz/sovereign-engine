# Sovereign Engine

Self-contained local AI inference platform. A Rust reverse proxy manages llama.cpp backend containers, provides OIDC authentication, fair-use scheduling, and a React dashboard — all in a single Docker image.

**Version:** 1.8.1

## Features

- **OpenAI-compatible API** — drop-in replacement at `/v1/*` for any client that speaks OpenAI
- **Multi-model support** — load and manage multiple models with GPU memory-aware scheduling
- **Multimodal / vision** — auto-detects and loads companion mmproj projectors for vision-capable GGUFs (e.g. Gemma 4)
- **Backend flexibility** — llama.cpp with NVIDIA CUDA, AMD ROCm, or CPU-only
- **OIDC authentication** — connect any identity provider; bootstrap mode for initial setup
- **API tokens** — per-user, SHA-256 hashed, manageable via dashboard or API
- **Fair-use scheduler** — per-user request queuing to prevent resource monopolisation
- **GPU reservation system** — exclusive-use time windows with admin approval workflow
- **React dashboard** — model management, user admin, usage metrics, HuggingFace model search
- **Open WebUI integration** — trusted-header SSO, no separate auth configuration needed
- **TLS** — manual certs or automatic provisioning via Let's Encrypt (ACME TLS-ALPN-01)
- **Network isolation** — dual Docker network architecture keeps backends unreachable from the host

## Architecture

```
                     ┌─────────────────────────────────────────────┐
                     │              sovereign-public               │
                     │            (host-facing bridge)             │
                     └────────────────────┬────────────────────────┘
                                          │
                Host ─────────────────► :3000 / :443
                                          │
                             ┌────────────┴────────────┐
                             │     Proxy (axum)        │
                             │                         │
                             │  api.* →                │
                             │    /v1/*    → OpenAI API│
                             │    /api/*   → Admin/User│
                             │    /portal/* → React SPA│
                             │  chat.* →               │
                             │    /*       → Open WebUI│
                             │                         │
                             │  [SQLite DB]            │
                             │  [React SPA static]     │
                             └────────────┬────────────┘
                                          │
                     ┌────────────────────┴────────────────────────┐
                     │            sovereign-internal               │
                     │        (isolated, backends only)            │
                     └───┬──────────────┬──────────────┬───────────┘
                         │              │              │
                  [backend :8080] [backend :8080] [backend :8080]
```

The proxy sits on both networks. Backend containers sit only on `sovereign-internal` and are never directly reachable from the host.

## Quick Start (Docker)

### Prerequisites

- Docker + Docker Compose
- GPU: NVIDIA (with [nvidia-container-toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/install-guide.html)), AMD ROCm, or CPU-only

### Docker Compose

Create a `docker-compose.yml`:

```yaml
services:
  proxy:
    image: dragonhold2024/sovereign-engine:1.0.0
    ports:
      - "3000:3000"
    networks:
      - sovereign-public
      - sovereign-internal
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
      - ./data:/config
      - ./models:/models
    environment:
      - LISTEN_ADDR=0.0.0.0:3000
      - DATABASE_URL=sqlite:///config/sovereign.db
      - BOOTSTRAP_USER=admin
      - BOOTSTRAP_PASSWORD=changeme
      - BREAK_GLASS=true
      - MODEL_PATH=/models
      - BACKEND_NETWORK=sovereign-internal
    restart: unless-stopped

networks:
  sovereign-public:
    name: sovereign-public
    driver: bridge
  sovereign-internal:
    name: sovereign-internal
    driver: bridge
    internal: true
```

Then:

```bash
docker compose up -d
```

Open [http://localhost:3000](http://localhost:3000) for Open WebUI, or [http://localhost:3000/portal](http://localhost:3000/portal) for the admin dashboard. On the login page, use the **"Break-glass / Admin emergency login"** form (visible because `BREAK_GLASS=true` is set) to log in with **admin** / **changeme** — this issues a session cookie.

> **Production note:** Set `BREAK_GLASS=false` after configuring an OIDC identity provider. Bootstrap credentials are intended for initial setup only.

### Verify Installation

```bash
curl -s http://localhost:3000/api/user/health | jq .
# Expected: {"status":"ok"}
```

### Next Steps

1. Download a model via the dashboard (HuggingFace search built in) or place GGUF files in `./models/`
2. Load a model from the Models page
3. Configure an Identity Provider via the admin panel or `/api/admin/idp`
4. Create API tokens for programmatic access
5. Point any OpenAI-compatible client at `http://localhost:3000/v1`

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:443` | Bind address for the proxy |
| `DATABASE_URL` | `sqlite:///config/sovereign.db` | SQLite database URL |
| `TLS_CERT_PATH` | _(none)_ | Path to TLS certificate PEM file |
| `TLS_KEY_PATH` | _(none)_ | Path to TLS private key PEM file |
| `ACME_CONTACT` | _(none)_ | Contact email for ACME; enables Let's Encrypt provisioning for both hostnames |
| `ACME_STAGING` | `false` | Use Let's Encrypt staging environment |
| `BOOTSTRAP_USER` | _(none)_ | Bootstrap admin username. Set with `BREAK_GLASS=true` to enable the break-glass login form. |
| `BOOTSTRAP_PASSWORD` | _(none)_ | Bootstrap admin password. Set with `BREAK_GLASS=true` to enable the break-glass login form. |
| `BREAK_GLASS` | `false` | When `true` (with `BOOTSTRAP_USER` + `BOOTSTRAP_PASSWORD` set), enables a one-shot break-glass login form on the portal login page (`POST /auth/bootstrap-login`). Not HTTP Basic auth — credentials are submitted once via the form and a session cookie is issued. Disable after OIDC is configured. |
| `DOCKER_HOST` | `unix:///var/run/docker.sock` | Docker socket path |
| `MODEL_PATH` | `/models` | Model storage path (inside the container) |
| `MODEL_HOST_PATH` | _(same as MODEL_PATH)_ | Host-side path for model bind mounts into child containers |
| `UI_PATH` | `/app/ui` | Path to static UI files |
| `API_HOSTNAME` | `localhost` | API subdomain hostname (e.g. `api.example.com`) |
| `CHAT_HOSTNAME` | `localhost` | Chat subdomain hostname (e.g. `chat.example.com`) |
| `COOKIE_DOMAIN` | _(none)_ | Cookie domain for cross-subdomain sessions (e.g. `.example.com`) |
| `BACKEND_NETWORK` | `sovereign-internal` | Docker network for backend container isolation |
| `WEBUI_BACKEND_URL` | `http://open-webui:8080` | Open WebUI backend URL (internal) |
| `WEBUI_API_KEY` | _(none)_ | Pre-shared key for Open WebUI → proxy `/v1` calls |
| `DB_ENCRYPTION_KEY` | _(none)_ | High-entropy random key for AES-256-GCM encryption of IdP client secrets at rest (e.g. `openssl rand -hex 32`; not a passphrase) |
| `SECURE_COOKIES` | `true` | Set `Secure` flag on session cookies (set `false` for HTTP dev) |
| `QUEUE_TIMEOUT_SECS` | `30` | Max seconds to hold a queued request before returning 429 |
| `RUST_LOG` | `sovereign_engine=info,tower_http=info` | Log level ([tracing EnvFilter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)) |

## Volumes

| Mount Point | Purpose |
|---|---|
| `/config` | SQLite database, ACME cert cache |
| `/models` | Model files (GGUF, etc.) — shared with backend containers |
| `/var/run/docker.sock` | Docker socket (required for backend container management) |

## TLS Configuration

**Option A: Automatic (Let's Encrypt)**

Set `API_HOSTNAME`, `CHAT_HOSTNAME`, `COOKIE_DOMAIN`, `ACME_CONTACT`, and `LISTEN_ADDR=0.0.0.0:443`. Port 443 must be directly reachable from the internet. Certs are cached in `/config/acme/` and cover both hostnames.

**Option B: Manual certs**

Set `TLS_CERT_PATH` and `TLS_KEY_PATH` to PEM files mounted into the container.

**Option C: No TLS (development)**

Set `LISTEN_ADDR=0.0.0.0:3000` and omit TLS variables. Not recommended for production.

## Development

### Prerequisites

- Rust 1.84+
- Node.js 22+
- Docker

### Build from Source

```bash
# Clone
git clone https://github.com/rauxon/SovereignEngine.git
cd SovereignEngine

# Rust proxy
cd proxy && cargo build --release

# React UI
cd ui && npm install && npm run build

# Full Docker image
docker build -t sovereign-engine .
```

### Local Development

1. Copy `.env.example` to `.env` and set `LISTEN_ADDR=0.0.0.0:31000`
2. Run the proxy: `cd proxy && cargo run`
3. Run the UI dev server: `cd ui && npm run dev`

The Vite dev server proxies `/api`, `/auth`, and `/v1` to `http://localhost:31000`.

## Documentation

- [User Guide](docs/USER_GUIDE.md)
- [Architecture](docs/ARCHITECTURE.md)
- [API Specification](docs/API.md)
- [Deployment Guide](docs/DEPLOYMENT.md)
- [Development Guide](docs/DEVELOPMENT.md)
- [Reservation System](docs/RESERVATIONS.md)
- [Contributing](CONTRIBUTING.md)
- [Code of Conduct](CODE_OF_CONDUCT.md)
- [Threat Model](docs/THREAT_MODEL.md)
- [Security Policy](SECURITY.md)
- [Changelog](CHANGELOG.md)
- [Architecture Decisions](docs/decisions/)

## License

Copyright 2026 Dragonhold. Licensed under the [Apache License 2.0](LICENSE).
