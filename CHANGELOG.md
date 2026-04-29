# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.7.0] - 2026-04-29

### Added
- **Backend supervisor:** automatic detection, capture, and recovery for crashed
  llama.cpp backend containers. A 10s tick task probes every loaded backend's
  `/health` endpoint; the proxy hot path also kicks the supervisor on any
  non-2xx outcome (connect failure or 5xx response) for immediate diagnosis.
- **Lifecycle FSM:** `Starting → Healthy → Suspect → Crashed → Quarantined`
  with per-model state tracked in memory. 5-min startup grace for new
  containers; 2 consecutive failures → `Crashed`.
- **Crash diagnostics:** before any container removal, exit code, OOMKilled
  flag, finished_at, and the last 500 log lines are captured. Log tails are
  written to `data/crash_logs/<model_id>-<unix_ts>.log` and indexed in a new
  `backend_crash_log` table. The crash-logs directory is GC'd inline to a
  1 GiB cap by oldest-mtime eviction.
- **Quarantine policy:** a backend that crashes without ever serving a
  successful response is quarantined (no auto-restart). A backend that
  served at least one 2xx response since its current container start is
  auto-restarted from persisted state — UID, api_key, gpu_type, gpu_layers,
  and parallel are reused so existing client sessions stay valid.
  Manual unquarantine = clicking the existing Start button (no separate
  endpoint or permission).
- **Admin UI:** per-model status badge expanded to
  `Loading | Healthy | Unhealthy | Crashed | Quarantined`. Crash history
  panel showing the last 5 events per model with timestamps, exit codes,
  OOMKilled flags, and links to view captured log tails (404 gracefully
  when GC'd).
- **`worked` flag:** new `models.worked` column tracks whether a backend
  instance has served any 2xx response. In-memory atomic on the hot path
  (one DB write per container start), DB column survives proxy restart.

### Changed
- `models.runtime_overrides` JSON shape now wrapped: `{cli: {...}, launch:
  {gpu_type, gpu_layers, parallel}}`. Legacy bare-shape blobs parse with
  backward compat. The `launch` sub-struct is what makes auto-restart
  possible — the supervisor cannot restart models with empty `launch`
  fields (e.g. containers started before this release); operator must
  manually restart them once post-deploy to populate the field.
- `recover_gate_state` now inspects each loaded model's container on
  proxy startup. If the container is gone, state is reconciled
  (`loaded=0`, gate not registered, basic crash row written) instead of
  re-registering a phantom gate slot.

### Migration notes
- Migration `20260429000000_supervisor_schema.sql` adds the new columns
  and `backend_crash_log` table. Forward-only; downgrade requires a
  hand-rolled DROP migration.
- On first deploy, currently-loaded backends will lack the `runtime_overrides.launch` sub-struct and will be skipped by auto-restart until manually
  restarted via the admin UI. The supervisor logs each skip.

## [1.6.1] - 2026-04-27

### Fixed
- llama-server is now launched with `--jinja`, activating the full Jinja2 chat-template path. Without it, modern model-native tool-call markup (qwen Hermes-style, gemma `<|tool_call>...`, mistral `[TOOL_CALLS]`, ...) leaked into `choices[0].message.content` instead of being parsed back into structured OpenAI `tool_calls` JSON — silently breaking every OpenAI-SDK consumer that relied on tool-calling against a Sovereign-served model.
- mmproj variant picker (`pick_mmproj_variant` in `proxy/src/main.rs` and `detect_mmproj_file` in `proxy/src/api/hf.rs`) is now case-insensitive. Bartowski ships lowercase quant tags (`mmproj-...-f16.gguf`) but unsloth ships uppercase (`mmproj-F16.gguf`, `mmproj-BF16.gguf`, `mmproj-F32.gguf`). With case-sensitive matching, all three preference branches missed for unsloth repos and the lex-first fallback picked `BF16` (because `'B' < 'F'`) rather than the documented `F16` preference. Functionally close (BF16 ≈ F16) but wrong per the documented contract.

## [1.6.0] - 2026-04-24

### Added
- First-class multimodal projector (mmproj) support for llama.cpp backends. Multimodal GGUFs (e.g. bartowski Gemma 4) now accept image input end-to-end through the proxy.
- New nullable `models.mmproj_filename` column tracks the companion `mmproj-*.gguf` / `mmproj_*.gguf` sibling that pairs with the main quant (migration adds the column; `Model` struct and all `SELECT`s updated to expose it).
- HuggingFace download flow auto-detects and fetches the preferred mmproj variant (f16 > bf16 > f32) alongside the main quant, bypassing any user-supplied `file_filter` so vision works out of the box; filename is persisted on the `models` row at ingestion.
- Startup backfill scans on-disk model directories and populates `mmproj_filename` for models downloaded before this release — no re-download required. Prefers f16 variants and skips rows that already have a value.
- `llama-server` is launched with `--mmproj /models/<path>` whenever a projector is present and readable; missing files degrade gracefully to text-only with a warning log.
- Admin UI surfaces a "Vision" badge on model rows whose projector is loaded, with a tooltip showing the projector filename.

### Changed
- `LlamacppConfig` gained an optional `mmproj_path` field; `start_llamacpp()` now delegates command construction to a pure `build_llamacpp_cmd` helper, making the flag wiring unit-testable without Docker.
- `AdminModel` TypeScript interface gained `mmproj_filename: string | null`.

### Fixed
- `api.agentics.org.nz` Gemma 4 deployments now accept image input. Previously returned HTTP 500 from llama.cpp with `"image input is not supported - hint: if this is unexpected, you may need to provide the mmproj"`. (Workflow card `019dbd5c-19b0-7ce2-85b9-834e20a3d88b`.)

## [1.5.2] - 2026-04-23

### Fixed
- VRAM estimation now correctly accounts for alternating global + sliding-window attention (Gemma 3 / Gemma 4). Previous formula assumed all layers used full context and the largest per-layer KV head count, leading to ~3× overestimates (e.g. Gemma 4 31B at 256K reported 120 GB KV cache vs ~20 GB actual) and incorrect "exceeds GPU" warnings that blocked model starts.

### Added
- GGUF reader now extracts `<arch>.attention.sliding_window_pattern` (per-layer bool array), `<arch>.attention.key_length_swa` / `value_length_swa`, and the full `<arch>.attention.head_count_kv` array (previously only the max was retained).
- New DB columns on `models`: `sliding_window`, `kv_bytes_per_token_global`, `kv_bytes_per_token_swa` — pre-aggregated at ingestion from the per-layer metadata so the estimator is a simple `(global_bpt × context + swa_bpt × min(context, window)) × parallel`.
- `backfill_gguf_metadata` (startup path) now populates the new columns plus `key_length` / `value_length` / `sliding_window` for already-ingested models that were missing them.
- 13 new unit tests covering GGUF parsing of the new fields, aggregate computation (Gemma 4-style heterogeneous + homogeneous fallback paths), and the SWA-aware estimator branch.

## [1.5.1] - 2026-04-23

### Fixed
- `DELETE /api/admin/models/:id` no longer returns a generic 500 when a model is pinned by an active token (SQLite FK violation on `tokens.specific_model_id`). The handler now pre-checks for blocking tokens and returns a structured 409 listing them; admins can retry with `?override=true` to soft-delete (revoke + `deleted_at`) the blockers and proceed. Filesystem removal moved to *after* the DB transaction commits, so a failed delete no longer leaves orphaned files or a stale DB row.
- Admin UI (`System` page) handles the 409 by surfacing the blocking tokens (name + user email) in a confirmation dialog with a "Revoke N tokens and delete" option.
- `ApiError` in the TypeScript client now preserves the parsed JSON error body on `.data`, so callers can inspect structured error payloads such as `blocking_tokens`.

## [1.5.0] - 2026-04-23

### Added
- Per-model `runtime_overrides` JSON column on the `models` table — admins can set llama-server CLI flags (`--cache-ram`, `--swa-full`, `-ctxcp`, `--cache-reuse`, plus free-form `extra`) per model via a new editor on the Model Mapping page (migration `20260423000000_model_runtime_overrides.sql`)
- Auto-detect at HuggingFace download-and-register time: SWA-bearing dense models (e.g. Gemma 3 31B) get `cache_ram_mib: 0` populated automatically as a stop-gap mitigation for upstream llama.cpp issue #21762
- GGUF metadata reader extracts `<arch>.attention.sliding_window` and `<arch>.expert_count`
- New `ModelRuntimeOverrides` Rust type with typed knobs and range/forbidden-prefix validation (19 unit tests); React `RuntimeOverridesEditor` component with live CLI preview (25 tests)

### Fixed
- Recurring `502 backend_unavailable` outages on Gemma 3 31B caused by `GGML_ASSERT(tensor->data != NULL)` in llama.cpp's prompt-cache save path (`state_write_data`) on dense + SWA models. Workaround disables the server-level prompt cache via `--cache-ram 0`. The bug is open upstream as `ggml-org/llama.cpp#21762` and is unresolved in builds up to b8882; once a fixed build is available, operators can clear the override via the admin UI to re-enable the cache
- `fetch_all_models` now selects `key_length` and `value_length` (previously absent — sqlx tolerated due to `Option<i64>` types)

## [1.4.2] - 2026-04-07

### Fixed
- GGUF reader now handles array-valued `attention.head_count_kv` (per-layer KV head counts) by taking the max across layers — fixes NULL metadata for heterogeneous-attention models like Gemma 4
- GGUF reader extracts `attention.key_length` and `attention.value_length` when present, stored in new DB columns
- VRAM estimator uses explicit key/value dimensions for KV cache calculation instead of deriving `head_dim = embedding_length / n_heads` — fixes significant undercount on models where these differ (e.g. Gemma 4)
- KV cache formula extracted into testable function with 10 new unit tests covering both parser and estimator

## [1.4.1] - 2026-04-07

### Fixed
- Model search now defaults to "any" task type instead of "text-generation", so multimodal models (e.g. Gemma v4, tagged `image-text-to-text`) appear in search results
- Added "any" option to the task filter dropdown in the model search UI

### Security
- Bumped `aws-lc-sys` 0.38.0 → 0.39.1 (via `aws-lc-rs` 1.16.2) — fixes GHSA-394x (X.509 name constraints bypass) and GHSA-9f94 (CRL scope check bypass)
- Bumped `lodash` 4.17.23 → 4.18.1 — fixes CVE-2026-4800 (arbitrary code execution via template imports)

## [1.4.0] - 2026-03-16

### Changed
- Container context size is now always determined by the model's `context_length` metadata from the database, removing the user-configurable context size parameter
- Start Model dialog shows context size as read-only instead of a dropdown selector
- VRAM estimation uses the model's stored context length instead of a user-supplied value
- Container start is rejected with a clear error if the model has no `context_length` set, preventing silent fallback to 4096

### Removed
- `context_size` parameter from admin container start API (`POST /api/admin/containers/start`)
- `context_size` parameter from reservation container start API (`POST /api/user/reservations/containers/start`)
- `context_size` parameter from VRAM estimate API (`POST /api/admin/containers/estimate`)

## [1.3.1] - 2026-03-16

### Fixed
- User guide: removed broken cross-doc links that didn't resolve in the in-app renderer at `/portal/guide`
- User guide: added Anthropic SDK usage examples (Python and Node.js) for the `/v1/messages` endpoint
- User guide: added Claude Code usage example with `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`
- User guide: updated available endpoints table to include `POST /v1/messages`
- Version bump to 1.3.1 across README, UI, and docs

## [1.3.0] - 2026-03-15

### Added
- Anthropic Messages API endpoint at `/v1/messages` with full format translation to OpenAI backend
- Streaming and non-streaming support for Anthropic Messages format
- `x-api-key` header authentication support (in addition to existing Bearer token auth)
- System prompts, multi-turn conversations, stop sequences, and tool definitions for Anthropic format
- End-to-end test suite for Anthropic endpoint (`tests/test_anthropic_endpoint.py`)

## [1.2.0] - 2026-03-15

### Added
- End-user guide covering portal navigation, chat interface, API tokens, model browsing, GPU reservations, and programmatic API usage (`docs/USER_GUIDE.md`)
- In-app user guide page at `/portal/guide` with themed markdown rendering and download button
- `react-markdown` and `remark-gfm` dependencies for markdown rendering in the portal
- Vite `@docs` alias for build-time import of documentation files
- Guide dynamically replaces placeholder URLs with the actual instance origin

### Changed
- Navigation bar: added "Guide" link between Reservations and Chat
- README: added User Guide to documentation links

## [1.1.0] - 2026-02-20

### Added
- Subdomain-based routing: `chat.<domain>` for Open WebUI, `api.<domain>` for portal and APIs (ADR 026)
- Host-based request dispatch via `Host` header with 421 Misdirected Request for unknown hosts
- Cross-subdomain cookie sharing via `Domain` attribute (`COOKIE_DOMAIN` env var)
- Multi-domain ACME certificate provisioning (SAN cert for both subdomains)
- DB encryption key rotation support via `DB_ENCRYPTION_KEY_OLD` env var
- Empty-key bug recovery: automatic migration from HKDF("") encrypted secrets
- 7 new migration tests covering all encryption key scenarios

### Changed
- OIDC callback redirects to `/portal/` instead of chat subdomain after login
- Session cookie validation now tries all matching cookies (handles stale duplicate cookies)
- Logout deletes all matching session cookies
- `EXTERNAL_URL` replaced by `API_HOSTNAME`, `CHAT_HOSTNAME`, `COOKIE_DOMAIN`
- `ACME_DOMAIN` removed; domains now derived from hostnames when `ACME_CONTACT` is set
- CORS allows both subdomain origins
- Dev mode preserved: when both hostnames are `localhost`, combined single-host router is used

### Fixed
- DB encryption: empty `DB_ENCRYPTION_KEY` was silently treated as a valid key, causing double-encryption when a real key was later set
- Token management: display bug, expiry support, soft delete
- API subdomain root (`/`) now redirects to `/portal/` instead of returning 404

### Security
- DB encryption key rotation without downtime (set old key, deploy new key, remove old key)
- Config now filters empty `DB_ENCRYPTION_KEY` to prevent accidental encryption with derived-from-nothing key

## [1.0.2] - 2026-02-18

### Added
- Comprehensive unit test suite: 92 Rust tests across 11 modules, 20 React tests
- Test coverage for security-critical paths: token hashing, encryption roundtrips, credential validation, CSP hash extraction
- Vitest + Testing Library setup for React UI

### Changed
- Reduced code duplication from 10.5% to 6.6%: extracted shared table/form styles, `TokenMintForm` component, and `proxy/src/api/common.rs` shared handlers
- Extracted `start_container_core()` and `post_stop_cleanup()` shared helpers to consolidate container lifecycle logic

## [1.0.1] - 2026-02-18

### Changed
- Converted all modal dialogs to native `<dialog>` element with `showModal()` for proper focus trapping, Escape key handling, and accessibility semantics
- Reduced Rust cognitive complexity: decomposed `download_single_file()` by extracting `hf_http_error_hint()` and `stream_response_to_file()` helpers
- Extracted shared `try_bootstrap_auth()` helper to eliminate duplicated Basic auth logic
- Replaced deprecated React 19 `FormEvent` usage with `SubmitEvent`
- Improved accessibility: proper label associations, keyboard support on interactive calendar elements, semantic HTML throughout
- Flattened deeply nested control flow in `fetch_tokenizer_config()` and `run_download()`
- Extracted `parse_drm_fdinfo_vram()` as a pure testable function

### Fixed
- Resolved 147 SonarQube code quality issues (code smells and bugs) across React UI and Rust proxy
- Fixed nested ternary expressions across multiple components
- Fixed missing `key` props using semantic identifiers instead of array indices
- Fixed non-interactive elements incorrectly receiving event handlers

## [1.0.0] - 2026-02-18

### Added
- GPU reservation system with admin approval workflow and automatic state transitions
- Per-container UID allocation and API key authentication for defence-in-depth
- AES-256-GCM encryption for IdP client secrets at rest
- Real-time metrics via SSE (GPU memory, CPU, disk, queue stats)
- Content Security Policy with SHA-256 inline script hashing
- Logarithmic fair-use scheduler with runtime-tunable parameters
- Concurrency gate with RAII slot management
- Meta tokens for Open WebUI per-user usage attribution
- Threat model documentation (docs/THREAT_MODEL.md)
- Architecture Decision Records (ADRs 001–024)
- CODE_OF_CONDUCT.md (Contributor Covenant)

### Changed
- Removed vLLM backend support; llama.cpp is now the sole backend (see ADR 001)
- Removed CUDA and ROCm backend support; Vulkan is now the sole GPU backend
- Updated security contact email in SECURITY.md
- Expanded CONTRIBUTING.md with GitHub fork/branch/PR workflow
- Expanded DEVELOPMENT.md with first-time contributor setup guide
- Added ADR index to ARCHITECTURE.md

### Security
- Fixed token scope bypass: category-scoped tokens no longer fall through to unrestricted model resolution
- Fixed HuggingFace download path traversal: file paths with `..` or leading `/` are rejected
- Fixed `hf_repo` directory traversal: format validation rejects `..` and non-standard characters
- Added constant-time comparison for bootstrap credentials (prevents timing side-channel)
- Added 10 MB request body size limit
- Added `Referrer-Policy` and `Permissions-Policy` security headers
- Migrated `DB_ENCRYPTION_KEY` derivation from bare SHA-256 to HKDF-SHA256 (automatic data migration on startup)
- Changed `BREAK_GLASS` default to `false` in docker-compose.yml; startup warns on default credentials
- Dockerfile now runs as non-root user (`sovereign`)

### Removed
- `docker-compose.nvidia.yml` and `docker-compose.rocm.yml` overlay files

## [0.9.0] - 2026-02-13

Initial public release preparation.

### Added
- Rust reverse proxy (axum) with OpenAI-compatible API passthrough
- Backend container management via Docker API (bollard) — llama.cpp with NVIDIA CUDA, AMD ROCm, or CPU-only
- OIDC authentication with PKCE and configurable identity providers
- Bootstrap credential authentication (break-glass mode)
- API token management (SHA-256 hashed, scoped per user, configurable expiry)
- Fair-use request scheduler with per-user queuing
- React dashboard with model management, user admin, and usage metrics
- Multi-model support with GPU memory-aware loading
- HuggingFace model search and background download with progress tracking
- TLS support: manual certs or automatic via Let's Encrypt (ACME TLS-ALPN-01)
- Open WebUI integration with trusted-header SSO
- Dual Docker network architecture (public + isolated internal)
- SQLite database with WAL mode and compile-time migration support
