# Sovereign Engine — API Specification

All API responses are JSON. Errors follow OpenAI's error format where applicable.

---

## Authentication

### Bearer Token (API access — `/v1/*`)
```
Authorization: Bearer se-<uuid>
```
Token is looked up by SHA-256 hash in `tokens` table. Resolves to user + permissions.

### Session Cookie (Portal — `/api/*`)
```
Cookie: se_session=<hex-token>
```
Set after OIDC login. 24-hour TTL. Looked up by SHA-256 hash in `sessions` table.

### Break-glass Login (`POST /auth/bootstrap-login`)

Active when `BREAK_GLASS=true`. Validates credentials against `BOOTSTRAP_USER` and `BOOTSTRAP_PASSWORD` env vars, then mints a session cookie. HTTP Basic Auth is not accepted anywhere.

**Request body:**
```json
{ "username": "string", "password": "string" }
```

**Responses:**
- `204 No Content` — credentials valid; `Set-Cookie: se_session=<token>` is included in the response. Use this cookie for all subsequent authenticated requests.
- `401 Unauthorized` — credentials invalid.
- `404 Not Found` — bootstrap is not active (`BREAK_GLASS` is not `true`).
- `429 Too Many Requests` — rate limit exceeded (5 requests/min/IP).

**Example:**
```bash
# Login and capture the session cookie
curl -c cookies.txt -s -o /dev/null -w "%{http_code}" \
  -X POST https://api.example.com/auth/bootstrap-login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"changeme"}'

# Use the cookie for subsequent calls
curl -b cookies.txt https://api.example.com/api/user/tokens | jq .
```

---

## Auth Routes (`/auth/*`) — No auth required

### `GET /auth/providers`
List enabled OIDC providers for the login page. Also signals whether the break-glass login form should be shown.

**Response 200:**
```json
{
  "providers": [
    { "id": "string", "name": "string" }
  ],
  "bootstrap_active": true
}
```

`bootstrap_active` is `true` when `BREAK_GLASS=true` is set server-side. The portal UI renders the break-glass form below the OIDC provider buttons when this field is `true`.

### `GET /auth/login?idp=<id>`
Redirects to the OIDC provider's authorization endpoint.

**Response 302:** Redirect to IdP.

### `GET /auth/callback?code=<code>&state=<state>`
Handles OIDC callback. Exchanges code for tokens, creates/updates user, sets session cookie.

**Response 302:** Redirect to `/` with `Set-Cookie: se_session=<token>`.

### `POST /auth/logout`
Clears session.

**Request:** (empty body, session cookie required)

**Response 200:**
```json
{ "status": "logged_out" }
```

### `GET /auth/me`
Returns current session user info. Used by the UI to check auth state.

**Response 200:**
```json
{
  "user_id": "string",
  "email": "string | null",
  "display_name": "string | null",
  "is_admin": true
}
```

**Response 401:** Not authenticated.

---

## User API (`/api/user/*`) — Session cookie required

### `GET /api/user/tokens`
List the authenticated user's API tokens (hashed — never returns plaintext).

**Response 200:**
```json
{
  "tokens": [
    {
      "id": "string",
      "name": "string",
      "category_id": "string | null",
      "category_name": "string | null",
      "specific_model_id": "string | null",
      "expires_at": "string | null",
      "revoked": false,
      "created_at": "string"
    }
  ]
}
```

### `POST /api/user/tokens`
Mint a new API token. Returns the plaintext token **once**.

**Request:**
```json
{
  "name": "string",
  "category_id": "string | null",
  "specific_model_id": "string | null",
  "expires_in_days": 90
}
```

`expires_in_days` is an integer (default 90). The token expires that many days from creation.

**Response 201:**
```json
{
  "token": "se-<uuid>",
  "name": "string",
  "warning": "Save this token — it cannot be shown again."
}
```

### `POST /api/user/tokens/:id/revoke`
Revoke a token.

**Response 200:**
```json
{ "status": "revoked" }
```

**Response 404:** Token not found or not owned by user.

### `GET /api/user/usage`
Usage statistics for the authenticated user.

**Query params:**
- `period` — `hour`, `day`, `week`, `month` (default: `day`)

**Response 200:**
```json
{
  "summary": {
    "total_requests": 0,
    "total_input_tokens": 0,
    "total_output_tokens": 0,
    "period": "day"
  },
  "by_model": [
    {
      "model_id": "string",
      "category_name": "string",
      "requests": 0,
      "input_tokens": 0,
      "output_tokens": 0
    }
  ],
  "by_token": [
    {
      "token_name": "string",
      "requests": 0,
      "input_tokens": 0,
      "output_tokens": 0
    }
  ]
}
```

### `GET /api/user/usage/timeline`
Time-series usage data broken down by model and token.

**Query params:**
- `period` — `hour`, `day`, `week`, `month` (default: `day`)

**Response 200:**
```json
{
  "timeline": [
    {
      "timestamp": "string",
      "model": "string",
      "requests": 0,
      "input_tokens": 0,
      "output_tokens": 0
    }
  ],
  "timeline_by_token": [
    {
      "timestamp": "string",
      "token_name": "string",
      "requests": 0,
      "input_tokens": 0,
      "output_tokens": 0
    }
  ]
}
```

### `GET /api/user/categories`
List available model categories (read-only for non-admins).

**Response 200:**
```json
{
  "categories": [
    {
      "id": "string",
      "name": "string",
      "description": "string",
      "preferred_model_id": "string | null",
      "created_at": "string"
    }
  ]
}
```

### `GET /api/user/models`
List all registered models (read-only for non-admins).

**Response 200:**
```json
{
  "models": [
    {
      "id": "string",
      "hf_repo": "string",
      "filename": "string | null",
      "size_bytes": 0,
      "category_id": "string | null",
      "loaded": false,
      "backend_type": "llamacpp",
      "context_length": 4096,
      "created_at": "string"
    }
  ]
}
```

### `GET /api/user/disk`
Disk usage for the model storage path.

**Response 200:**
```json
{
  "total_bytes": 0,
  "used_bytes": 0,
  "free_bytes": 0
}
```

### `GET /api/user/events` (SSE)
Unified Server-Sent Events stream merging metrics and reservation signals.

**Event types:**

- **`metrics`** (every ~2s) — system metrics snapshot
  - Admin payload: full `MetricsSnapshot` (GPU memory, CPU, disk, queues, containers, active reservation)
  - Non-admin payload: `{ gpu_memory, active_reservation, timestamp }`

- **`reservations_changed`** — emitted on any reservation state change (no data payload)

**Example:**
```
event: metrics
data: {"gpu_memory":{"total_mb":32768,"used_mb":8192},"timestamp":"2026-02-17T10:00:00Z",...}

event: reservations_changed

```

Clients should reconnect on disconnection. The stream uses SSE keep-alive.

---

## Reservations API

### User Routes (`/api/user/*`)

#### `POST /api/user/reservations`
Create a new reservation request. Times must be on 30-minute boundaries and in the future.

**Request:**
```json
{
  "start_time": "2026-02-20T14:00:00",
  "end_time": "2026-02-20T18:00:00",
  "reason": "Batch inference job"
}
```

**Response 201:**
```json
{ "id": "uuid", "status": "pending" }
```

**Response 400:** Invalid times, not on 30-min boundary, end before start, or in the past.
**Response 409:** Overlaps with an existing approved/active reservation.

#### `GET /api/user/reservations`
List the current user's reservations (all statuses).

**Response 200:**
```json
{
  "reservations": [
    {
      "id": "uuid",
      "user_id": "uuid",
      "status": "pending | approved | active | completed | rejected | cancelled",
      "start_time": "string",
      "end_time": "string",
      "reason": "string",
      "admin_note": "string",
      "approved_by": "uuid | null",
      "created_at": "string",
      "updated_at": "string"
    }
  ]
}
```

#### `POST /api/user/reservations/:id/cancel`
Cancel own pending or approved reservation.

**Response 200:**
```json
{ "status": "cancelled" }
```

**Response 404:** Not found, not owned by user, or not in a cancellable state.

#### `GET /api/user/reservations/active`
Get the currently active reservation (if any). Visible to all authenticated users.

**Response 200:**
```json
{
  "active": true,
  "reservation_id": "uuid",
  "user_id": "uuid",
  "user_display_name": "string | null",
  "end_time": "string"
}
```
Or `{ "active": false }` when no reservation is active.

#### `GET /api/user/reservations/calendar`
All approved, active, and pending reservations for calendar display (all users).

**Response 200:**
```json
{
  "reservations": [
    {
      "id": "uuid",
      "user_id": "uuid",
      "status": "string",
      "start_time": "string",
      "end_time": "string",
      "reason": "string",
      "user_email": "string | null",
      "user_display_name": "string | null"
    }
  ]
}
```

#### `POST /api/user/reservations/containers/start`
Start a container during the active reservation (reservation holder only).

**Request:**
```json
{
  "model_id": "uuid",
  "backend_type": "llamacpp",
  "gpu_type": "rocm | cuda | none",
  "gpu_layers": 99,
  "context_size": 4096,
  "parallel": 1
}
```

Only `model_id` is required; other fields have defaults.

**Response 200:**
```json
{
  "container": "sovereign-llamacpp-<model_id>",
  "url": "http://sovereign-llamacpp-<model_id>:8080"
}
```

**Response 403:** Caller does not hold the active reservation.

#### `POST /api/user/reservations/containers/stop`
Stop a container during the active reservation (reservation holder only).

**Request:**
```json
{ "model_id": "uuid" }
```

**Response 200:**
```json
{ "status": "stopped" }
```

### Admin Routes (`/api/admin/*`)

#### `GET /api/admin/reservations`
List all reservations with user display info.

**Response 200:** Same shape as user listing but includes all users' reservations.

#### `POST /api/admin/reservations/:id/approve`
Approve a pending reservation. Checks for overlap before approving.

**Request:**
```json
{ "note": "Optional admin note" }
```

**Response 200:**
```json
{ "status": "approved" }
```

**Response 409:** Approving would create an overlap.

#### `POST /api/admin/reservations/:id/reject`
Reject a pending reservation.

**Request:**
```json
{ "note": "Optional rejection reason" }
```

**Response 200:**
```json
{ "status": "rejected" }
```

#### `POST /api/admin/reservations/:id/activate`
Force-activate an approved reservation immediately.

**Response 200:**
```json
{ "status": "active" }
```

**Response 409:** Another reservation is already active.

#### `POST /api/admin/reservations/:id/deactivate`
Force-end an active reservation early.

**Response 200:**
```json
{ "status": "completed" }
```

#### `DELETE /api/admin/reservations/:id`
Delete a reservation record. Cannot delete active reservations (deactivate first).

**Response 200:**
```json
{ "status": "deleted" }
```

---

## Settings API (`/api/admin/*`)

### `GET /api/admin/settings`
Return current fairness/queue settings.

**Response 200:**
```json
{
  "fairness_base_priority": 100.0,
  "fairness_wait_weight": 1.0,
  "fairness_usage_weight": 10.0,
  "fairness_usage_scale": 1000.0,
  "fairness_window_minutes": 60,
  "queue_timeout_secs": 30
}
```

### `PUT /api/admin/settings`
Partial update — only the provided keys are changed.

**Request:**
```json
{
  "fairness_base_priority": 200.0,
  "queue_timeout_secs": 60
}
```

**Response 200:** Returns the full updated settings object (same shape as GET).

---

## Admin API (`/api/admin/*`) — Session auth + admin role required

### Identity Providers

#### `GET /api/admin/idps`
List all configured IdPs.

**Response 200:**
```json
{
  "idps": [
    {
      "id": "string",
      "name": "string",
      "issuer": "string",
      "client_id": "string",
      "scopes": "string",
      "enabled": true,
      "created_at": "string"
    }
  ]
}
```

#### `POST /api/admin/idps`
Add a new OIDC provider.

**Request:**
```json
{
  "name": "string",
  "issuer": "string",
  "client_id": "string",
  "client_secret": "string",
  "scopes": "openid email profile"
}
```

**Response 201:**
```json
{
  "id": "string",
  "name": "string"
}
```

#### `PUT /api/admin/idps/:id`
Update an IdP configuration.

**Request:** Same fields as POST (all optional).

**Response 200:**
```json
{ "status": "updated" }
```

#### `DELETE /api/admin/idps/:id`
Disable an IdP (soft delete — sets `enabled = 0`).

**Response 200:**
```json
{ "status": "disabled" }
```

### Model Categories

#### `GET /api/admin/categories`
**Response 200:**
```json
{
  "categories": [
    {
      "id": "string",
      "name": "string",
      "description": "string",
      "preferred_model_id": "string | null",
      "created_at": "string"
    }
  ]
}
```

#### `POST /api/admin/categories`
**Request:**
```json
{
  "name": "string",
  "description": "string",
  "preferred_model_id": "string | null"
}
```

**Response 201:**
```json
{ "id": "string", "name": "string" }
```

#### `PUT /api/admin/categories/:id`
**Request:** Same fields as POST (all optional).

**Response 200:**
```json
{ "status": "updated" }
```

#### `DELETE /api/admin/categories/:id`
**Response 200:**
```json
{ "status": "deleted" }
```

### Models

#### `GET /api/admin/models`
**Response 200:**
```json
{
  "models": [
    {
      "id": "string",
      "hf_repo": "string",
      "filename": "string | null",
      "size_bytes": 0,
      "category_id": "string | null",
      "loaded": false,
      "backend_type": "llamacpp",
      "last_used_at": "string | null",
      "created_at": "string"
    }
  ]
}
```

#### `POST /api/admin/models`
Register a model (does not download or start it).

**Request:**
```json
{
  "hf_repo": "string",
  "category_id": "string | null"
}
```

**Response 201:**
```json
{ "id": "string", "hf_repo": "string" }
```

#### `PUT /api/admin/models/:id`
Update model metadata (e.g. assign to category).

**Request:**
```json
{
  "category_id": "string | null"
}
```

**Response 200:**
```json
{ "status": "updated" }
```

#### `DELETE /api/admin/models/:id`
Unregister a model (must be unloaded first).

**Response 200:**
```json
{ "status": "deleted" }
```

**Response 409:** Model is currently loaded.

### Containers (backend lifecycle)

#### `GET /api/admin/containers`
List all managed backend containers.

**Response 200:**
```json
{
  "containers": [
    {
      "id": "string",
      "names": ["string"],
      "state": "running | exited | ...",
      "status": "string",
      "labels": {}
    }
  ]
}
```

> Container model IDs can be found in the `labels` field under the key `sovereign-engine.model-id`. Containers are named `sovereign-llamacpp-{model_id}`.

#### `POST /api/admin/containers/start`
Start a backend container for a model.

**Request:**
```json
{
  "model_id": "string",
  "gpu_type": "rocm | cuda | none",
  "gpu_layers": 99,
  "context_size": 4096,
  "parallel": 1
}
```

> Backend containers are attached to the internal Docker network (`sovereign-internal`) and are not exposed on any host port. The proxy reaches them by container name.

**Response 200:**
```json
{
  "container": "sovereign-llamacpp-<model_id>",
  "url": "http://sovereign-llamacpp-<model_id>:8080"
}
```

#### `POST /api/admin/containers/stop`
Stop and remove a backend container.

**Request:**
```json
{
  "model_id": "string"
}
```

**Response 200:**
```json
{ "status": "stopped" }
```

### Users

#### `GET /api/admin/users`
**Response 200:**
```json
{
  "users": [
    {
      "id": "string",
      "idp_id": "string",
      "email": "string | null",
      "display_name": "string | null",
      "is_admin": false,
      "created_at": "string",
      "usage_summary": {
        "total_requests": 0,
        "total_tokens": 0
      }
    }
  ]
}
```

#### `PUT /api/admin/users/:id`
Update user (toggle admin, etc).

**Request:**
```json
{
  "is_admin": true
}
```

**Response 200:**
```json
{ "status": "updated" }
```

### System

#### `GET /api/admin/system`
System overview: disk, queue depth, container health.

**Response 200:**
```json
{
  "disk": {
    "model_path": "/models",
    "total_bytes": 0,
    "used_bytes": 0,
    "free_bytes": 0
  },
  "queues": {
    "category_name": { "depth": 0, "avg_wait_ms": 0 }
  },
  "containers": [
    {
      "model_id": "string",
      "healthy": true,
      "uptime_seconds": 0
    }
  ]
}
```

### IdP Model Access Mappings

#### `GET /api/admin/access-mappings`
**Response 200:**
```json
{
  "mappings": [
    {
      "id": "string",
      "idp_id": "string",
      "group_claim": "string",
      "group_value": "string",
      "category_id": "string"
    }
  ]
}
```

#### `POST /api/admin/access-mappings`
**Request:**
```json
{
  "idp_id": "string",
  "group_claim": "string",
  "group_value": "string",
  "category_id": "string"
}
```

**Response 201:**
```json
{ "id": "string" }
```

#### `DELETE /api/admin/access-mappings/:id`
**Response 200:**
```json
{ "status": "deleted" }
```

---

## OpenAI-Compatible API (`/v1/*`) — Bearer token required

These follow the [OpenAI API specification](https://platform.openai.com/docs/api-reference).

### `GET /v1/models`
List loaded models.

**Response 200:**
```json
{
  "object": "list",
  "data": [
    {
      "id": "string",
      "object": "model",
      "owned_by": "sovereign-engine"
    }
  ]
}
```

### `POST /v1/chat/completions`
Chat completion. Body is passed through to the llama.cpp backend.

**Request:** Standard OpenAI ChatCompletion request. The `model` field can be:
- A model category name (e.g. `"thinking"`) — resolved to preferred model
- A specific model ID — used directly

**Response 200:** Standard OpenAI ChatCompletion response (or SSE stream if `stream: true`).

### `POST /v1/completions`
Text completion. Same routing logic as chat completions.

---

## HuggingFace Integration (`/api/admin/hf/*`) — Admin only

### `GET /api/admin/hf/search?q=<query>&task=text-generation`
Search HuggingFace models.

**Response 200:**
```json
{
  "models": [
    {
      "id": "org/model-name",
      "downloads": 0,
      "likes": 0,
      "pipeline_tag": "text-generation",
      "tags": ["string"]
    }
  ]
}
```

### `POST /api/admin/hf/download`
Start downloading a model from HuggingFace.

**Request:**
```json
{
  "hf_repo": "string",
  "category_id": "string | null"
}
```

**Response 202:**
```json
{
  "download_id": "string",
  "status": "started"
}
```

### `GET /api/admin/hf/downloads`
List active/recent downloads.

**Response 200:**
```json
{
  "downloads": [
    {
      "id": "string",
      "hf_repo": "string",
      "progress_bytes": 0,
      "total_bytes": 0,
      "status": "downloading | complete | failed",
      "error": "string | null"
    }
  ]
}
```

### `DELETE /api/admin/hf/downloads/:id`
Cancel an active download.

**Response 200:**
```json
{ "status": "cancelled" }
```

---

## Error Format

All errors follow this structure:
```json
{
  "error": {
    "message": "Human-readable description",
    "type": "invalid_request_error | server_error | auth_error",
    "code": "machine_readable_code"
  }
}
```

For non-OpenAI routes, a simplified form is also acceptable:
```json
{
  "error": "Human-readable description"
}
```
