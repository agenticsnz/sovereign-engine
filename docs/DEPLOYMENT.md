# Sovereign Engine — Deployment Guide

---

## Docker Compose Quickstart (Dev Mode)

The default `docker-compose.yml` runs on HTTP (port 3000) for easy local development.

```bash
# Clone and start
git clone <repo-url>
cd SovereignEngine
docker compose up --build

# Open in browser
open http://localhost:3000/portal/
```

What happens on first start:
- SQLite database created at `/config/sovereign.db` (in Docker volume `sovereign-config`)
- Migrations run automatically
- Bootstrap auth is **not** active by default (`BREAK_GLASS=false`). Set `BREAK_GLASS=true` (plus `BOOTSTRAP_USER` and `BOOTSTRAP_PASSWORD`) in your `.env` for initial setup.
- Docker networks `sovereign-public`, `sovereign-internal`, and `sovereign-openwebui` created

**First login (break-glass):**

1. Set `BREAK_GLASS=true`, `BOOTSTRAP_USER`, and `BOOTSTRAP_PASSWORD` in `.env` and restart.
2. Open `https://<API_HOSTNAME>/portal/` (or `http://localhost:3000/portal/` in dev mode) in a browser.
3. The login page shows a **"Break-glass / Admin emergency login"** form below any configured OIDC providers.
4. Enter the bootstrap credentials and submit — the proxy validates them, issues a session cookie, and redirects you to the portal.
5. Configure an OIDC provider via the admin UI (see [OIDC Provider Configuration](#oidc-provider-configuration) below).
6. Once OIDC is working, set `BREAK_GLASS=false` (or unset) and restart. The break-glass form will no longer appear.

---

## Docker Socket Access

The proxy container runs as a non-root user (`sovereign`). It needs access to the Docker socket to manage backend containers. The `docker-compose.yml` adds the Docker socket group via `group_add`:

```yaml
group_add:
  - "${DOCKER_GID:-999}"
```

If your host's Docker socket uses a different GID (common values: 998, 999, docker), check with:

```bash
stat -c %g /var/run/docker.sock
```

Override in your `.env` if needed:

```bash
DOCKER_GID=998
```

---

## GPU Support

GPU acceleration uses the Vulkan backend. The proxy auto-detects `/dev/dri` and exposes it to backend containers.

Ensure the proxy container has access to the GPU devices:

```yaml
devices:
  - /dev/dri
  - /dev/kfd    # AMD only — omit for NVIDIA
```

The proxy automatically discovers the GIDs that own the GPU device files and forwards them to backend containers. No manual group ID configuration is needed for GPU access.

---

## Production TLS Setup

1. **Generate certificates** (self-signed for testing, or use real certs):
   ```bash
   ./scripts/generate-dev-certs.sh
   # Creates config/cert.pem and config/key.pem
   ```

2. **Update docker-compose.yml environment:**
   ```yaml
   environment:
     - TLS_CERT_PATH=/config/cert.pem
     - TLS_KEY_PATH=/config/key.pem
     - LISTEN_ADDR=0.0.0.0:443
     - API_HOSTNAME=api.example.com
     - CHAT_HOSTNAME=chat.example.com
     - COOKIE_DOMAIN=.example.com
     - SECURE_COOKIES=true
   ```

3. **Update port mapping:**
   ```yaml
   ports:
     - "443:443"
   ```

4. **Mount certificate directory:**
   ```yaml
   volumes:
     - ./config:/config  # Contains cert.pem and key.pem
   ```

The proxy auto-detects TLS mode in priority order:
1. **ACME** — if `ACME_CONTACT` is set, provisions certs automatically via Let's Encrypt (TLS-ALPN-01) for both `API_HOSTNAME` and `CHAT_HOSTNAME`. Requires port 443 reachable from the internet. Certs cached in `/config/acme/`.
2. **Manual TLS** — if `TLS_CERT_PATH` and `TLS_KEY_PATH` are set, loads PEM files.
3. **HTTP** — otherwise, plain HTTP.

---

## OIDC Provider Configuration

Step-by-step for adding an identity provider:

1. **Log in via the break-glass form** (see First login above) to obtain an admin session cookie.

2. **Register your IdP** using the admin UI, or via curl with your session cookie:
   ```bash
   # Replace <token> with the value of your se_session cookie
   curl -H "Cookie: se_session=<token>" -X POST http://localhost:3000/api/admin/idps \
     -H "Content-Type: application/json" \
     -d '{
       "name": "My Company SSO",
       "issuer": "https://accounts.google.com",
       "client_id": "your-client-id.apps.googleusercontent.com",
       "client_secret": "your-client-secret",
       "scopes": "openid email profile"
     }'
   ```

3. **Configure your IdP's redirect URI:**
   Set the authorized redirect URI in your IdP to: `https://<API_HOSTNAME>/auth/callback`
   Example: `https://api.example.com/auth/callback` (or `http://localhost:3000/auth/callback` in dev mode)

4. **Test the login flow:**
   - Open the app in a browser
   - The IdP should appear as a login button
   - Click to initiate OIDC login
   - After callback, you'll be logged in with a session cookie

5. **Once an IdP is configured:**
   - New users are created automatically on first OIDC login
   - First OIDC user is automatically promoted to admin
   - Set `BREAK_GLASS=false` and restart once OIDC is working

---

## Model Management

All admin API calls require a session cookie (obtained via OIDC login or the break-glass form). Replace `<token>` with your `se_session` cookie value.

**Search HuggingFace models:**
```bash
curl -H "Cookie: se_session=<token>" \
  "http://localhost:3000/api/admin/hf/search?q=llama&task=text-generation"
```

**Download a model:**
```bash
curl -H "Cookie: se_session=<token>" -X POST http://localhost:3000/api/admin/hf/download \
  -H "Content-Type: application/json" \
  -d '{"hf_repo": "meta-llama/Llama-3-8B", "category_id": null}'
```

**Check download progress:**
```bash
curl -H "Cookie: se_session=<token>" http://localhost:3000/api/admin/hf/downloads
```

**Create a model category:**
```bash
curl -H "Cookie: se_session=<token>" -X POST http://localhost:3000/api/admin/categories \
  -H "Content-Type: application/json" \
  -d '{"name": "thinking", "description": "Models for reasoning tasks"}'
```

**Start a backend container:**
```bash
curl -H "Cookie: se_session=<token>" -X POST http://localhost:3000/api/admin/containers/start \
  -H "Content-Type: application/json" \
  -d '{"model_id": "<model-uuid>"}'
```

**Stop a container:**
```bash
curl -H "Cookie: se_session=<token>" -X POST http://localhost:3000/api/admin/containers/stop \
  -H "Content-Type: application/json" \
  -d '{"model_id": "<model-uuid>"}'
```

---

## Network Isolation

The `sovereign-internal` Docker network is created with `internal: true`:
- Backend containers **cannot** reach the internet or the host
- Backend containers **cannot** bind ports to the host
- Only the proxy (which is on both networks) can communicate with backends
- Proxy reaches backends by container name: `http://sovereign-llamacpp-{model_id}:8080`

**UID isolation:** Each backend container runs as a unique non-root user (UID randomly allocated in 10000–65000 with collision avoidance). This prevents cross-container process interference.

**Model files:** Mounted read-only into backend containers from the shared `/models` volume.

---

## Monitoring

**Health check:**
The docker-compose.yml includes a health check. For a quick manual check:
```bash
curl -ksf https://localhost:3000/auth/providers || echo "unhealthy"
```

If using TLS:
```bash
curl -ksf https://localhost:443/auth/providers || echo "unhealthy"
```

**SSE Metrics Stream:**
Real-time system metrics are available via Server-Sent Events:
```bash
curl -H "Cookie: se_session=<token>" \
  http://localhost:3000/api/user/events
```

Admin sessions receive full metrics (GPU memory, CPU, disk, queues, containers). Non-admin sessions receive GPU memory and active reservation status. See [API docs](API.md#get-apiuserevents-sse) for details.

**Logging:**
Control log verbosity with `RUST_LOG`:
```bash
# Default
RUST_LOG=sovereign_engine=info,tower_http=info

# Debug (verbose request logging)
RUST_LOG=sovereign_engine=debug,tower_http=debug

# Trace (maximum verbosity)
RUST_LOG=sovereign_engine=trace,tower_http=trace
```

Logs use `tracing` with structured output. Each request logged by `tower_http::TraceLayer`.

**System status API:**
```bash
curl -H "Cookie: se_session=<token>" http://localhost:3000/api/admin/system
```
Returns disk usage (model_path filesystem), container health (running/stopped), and queue depths.

**Usage queries:**
User-facing usage endpoint with period aggregation:
```bash
# For the current user
curl -H "Cookie: se_session=<token>" \
  "http://localhost:3000/api/user/usage?period=day"
# Periods: hour, day, week, month
```

For admin-level usage analysis, query the SQLite database directly:
```sql
-- Top users by request count (last 24h)
SELECT user_id, COUNT(*) as requests, SUM(input_tokens + output_tokens) as total_tokens
FROM usage_log
WHERE created_at > datetime('now', '-1 day')
GROUP BY user_id ORDER BY requests DESC;

-- Model usage breakdown
SELECT model_id, category_id, COUNT(*) as requests, AVG(latency_ms) as avg_latency
FROM usage_log
WHERE created_at > datetime('now', '-7 days')
GROUP BY model_id;
```

---

## Backup & Recovery

**Database:**
- SQLite with WAL mode — safe to copy while the server is running (WAL provides read consistency)
- Database file: `/config/sovereign.db` (in the `sovereign-config` Docker volume)
- Back up the entire `/config` volume:
  ```bash
  docker cp $(docker compose ps -q proxy):/config ./config-backup
  ```

**Models:**
- Model files in `/models` volume. Large files — back up independently or re-download from HuggingFace.

**Break-glass mode:**
If you lose access to all OIDC IdPs or sessions:
1. Set `BREAK_GLASS=true`, `BOOTSTRAP_USER`, and `BOOTSTRAP_PASSWORD` in the environment.
2. Restart the container.
3. Open `https://<API_HOSTNAME>/portal/` in a browser.
4. The login page shows a "Break-glass / Admin emergency login" form. Enter the bootstrap credentials and submit.
5. You now have an admin session cookie — reconfigure IdPs via the admin UI as needed.
6. Set `BREAK_GLASS=false` (or unset) and restart. The break-glass form will no longer appear.

---

## Security Considerations

- **Token hashing:** API tokens are stored as SHA-256 hashes. Plaintext shown only once at creation.
- **Session TTL:** Sessions expire after 24 hours. Stored as SHA-256 hashed tokens.
- **OIDC security:** CSRF token + nonce + PKCE stored in `oidc_auth_state` table. Validated on callback.
- **Secret encryption:** IdP client secrets optionally encrypted at rest with AES-256-GCM (set `DB_ENCRYPTION_KEY`).
- **No secrets in image:** All secrets (bootstrap creds, IdP client_secret, TLS keys, encryption key) are passed via environment variables or volume mounts.
- **Network isolation:** Backend containers have no host access. Model files mounted read-only.
- **UID isolation:** Each backend container runs as a different non-root user (random allocation with collision avoidance).
- **CSP:** Content Security Policy with per-build SHA-256 hashes for inline scripts (computed at startup).
- **Docker socket:** The proxy needs access to the Docker socket (`/var/run/docker.sock`) to manage backend containers. This grants significant privileges — restrict access to the container in production.
