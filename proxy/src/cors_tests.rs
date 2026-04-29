//! CORS layer regression tests.
//!
//! Pins the behaviour of the two CORS layers introduced in Phase 3:
//!
//! - **Bearer layer** (`build_bearer_cors_layer`): `AllowOrigin::any()`, credentials
//!   disabled. Applied to `/v1/*` (OpenAI + Anthropic compat routes).
//! - **Strict layer** (`build_strict_cors_layer`): allow-list of
//!   `api_external_url()` + `chat_external_url()`, `allow_credentials(true)`.
//!   Applied to `/auth/*`, `/api/*`, `/portal/*`, and the WebUI fallback.
//!
//! # Test groups
//!
//! ## Bearer layer (`/v1/*`)
//! 1. `bearer_allowed_claude_ai` — arbitrary origin (claude.ai) → `*`; no credentials.
//! 2. `bearer_allowed_api_hostname` — `api_external_url()` origin → matched.
//! 3. `bearer_allowed_localhost` — `http://localhost:8080` → matched.
//! 4. `bearer_allowed_null_origin` — `null` (file://) → matched.
//! 5. `bearer_anthropic_endpoint` — same policy on `OPTIONS /v1/messages`.
//! 6. `bearer_preflight_no_auth_required` — `OPTIONS /v1/chat/completions` without
//!    Authorization → 200/204 (preflight short-circuits before auth middleware).
//! 7. `bearer_actual_request_cors_headers` — `POST /v1/...` with no token → 401,
//!    but CORS headers still present on the error response.
//!
//! ## Strict layer (`/api/*` and `/auth/*`)
//! 8.  `strict_allowed_api_hostname` — `api_external_url()` → matched; credentials true.
//! 9.  `strict_allowed_chat_hostname` — `chat_external_url()` → matched; credentials true.
//! 10. `strict_disallowed_claude_ai` — `https://claude.ai` → no `Access-Control-Allow-Origin`.
//! 11. `strict_disallowed_null_origin` — `null` → not matched.
//! 12. `strict_auth_routes_positive` — `OPTIONS /auth/providers` from `api_external_url()` → matched.
//! 13. `strict_auth_routes_negative` — `OPTIONS /auth/providers` from evil origin → blocked.
//!
//! ## Layer isolation
//! 14. `isolation_v1_no_credentials` — `/v1/*` with a strict-allowed origin → no credentials header.
//! 15. `isolation_api_no_wildcard` — `/api/*` with an arbitrary origin → no `Allow-Origin`.
//!
//! # tower-http behaviour verified experimentally
//!
//! - `AllowOrigin::any()`: emits `Access-Control-Allow-Origin: *` (not echoed origin).
//! - `AllowOrigin::list([...])` on match: echoes the request Origin value.
//! - `AllowOrigin::list([...])` on no-match: omits `Access-Control-Allow-Origin`.
//!   However, `Access-Control-Allow-Credentials` is still emitted on no-match (tower-http 0.6
//!   writes it unconditionally when `allow_credentials(true)` is set). The browser still
//!   blocks the request because `Access-Control-Allow-Origin` is absent, so this is safe —
//!   but the credentials header itself is present even for blocked preflights.
//! - `Vary: Origin`: emitted for both `AllowOrigin::any()` and `AllowOrigin::list(...)`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::Router;
use tower::ServiceExt;

use crate::api::{anthropic, openai};
use crate::auth::{self, oidc};
use crate::config::AppConfig;
use crate::db::Database;
use crate::docker::DockerManager;
use crate::metrics::MetricsBroadcaster;
use crate::scheduler::reservation::ReservationBroadcaster;
use crate::scheduler::Scheduler;
use crate::{build_bearer_cors_layer, build_strict_cors_layer, AppState};

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

/// AppConfig with distinct api/chat hostnames so strict-layer tests can
/// verify both allow-list entries. `secure_cookies: false` keeps scheme
/// as `http` so `api_external_url()` → `http://api.test.local`.
fn test_config() -> AppConfig {
    AppConfig {
        listen_addr: "127.0.0.1:0".to_string(),
        database_url: "sqlite::memory:".to_string(),
        tls_cert_path: None,
        tls_key_path: None,
        bootstrap_user: None,
        bootstrap_password: None,
        break_glass: false,
        docker_host: "unix:///var/run/docker.sock".to_string(),
        model_path: "/tmp/test-models".to_string(),
        model_host_path: "/tmp/test-models".to_string(),
        ui_path: "/tmp/test-ui".to_string(),
        api_hostname: "api.test.local".to_string(),
        chat_hostname: "chat.test.local".to_string(),
        cookie_domain: None,
        backend_network: "test-network".to_string(),
        acme_contact: None,
        acme_staging: false,
        webui_backend_url: "http://localhost:8080".to_string(),
        webui_api_key: None,
        queue_timeout_secs: 30,
        // false → http:// scheme, so api_external_url() = "http://api.test.local"
        secure_cookies: false,
        db_encryption_key: None,
        db_encryption_key_old: None,
        data_path: "/tmp/test-data-path".to_string(),
    }
}

async fn test_app_state() -> Arc<AppState> {
    let db = Database::test_db().await;
    let (probe_tx, _probe_rx) = crate::supervisor::channel();
    Arc::new(AppState {
        config: test_config(),
        db,
        docker: DockerManager::test_dummy(),
        scheduler: Scheduler::new(),
        metrics: MetricsBroadcaster::new(),
        reservations: ReservationBroadcaster::new(),
        supervisor_map: std::sync::Arc::new(dashmap::DashMap::new()),
        probe_tx,
        worked_map: std::sync::Arc::new(dashmap::DashMap::new()),
    })
}

/// Build a `/v1/*` router with bearer CORS and real bearer_auth_middleware.
/// Mirrors the wiring in `build_router()` (main.rs ~line 561).
fn bearer_router(state: Arc<AppState>) -> Router {
    let bearer_cors = build_bearer_cors_layer();
    let openai_routes = openai::routes(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::bearer_auth_middleware,
        ))
        .layer(bearer_cors.clone());
    let anthropic_routes = anthropic::routes(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::bearer_auth_middleware,
        ))
        .layer(bearer_cors);
    Router::new()
        .nest("/v1", openai_routes)
        .nest("/v1", anthropic_routes)
}

/// Build an `/api/*` + `/auth/*` router with strict CORS.
/// Mirrors the wiring in `build_router()` (main.rs ~line 548–556).
fn strict_router(state: Arc<AppState>) -> Router {
    let strict_cors = build_strict_cors_layer(&state.config);
    let auth_routes = oidc::routes(state.clone()).layer(strict_cors.clone());
    let api_routes = crate::api::routes(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::session_auth_middleware,
        ))
        .layer(strict_cors);
    Router::new()
        .nest("/auth", auth_routes)
        .nest("/api", api_routes)
}

/// Fire an `OPTIONS` preflight at the given router and return the response.
async fn preflight(
    router: Router,
    uri: &str,
    origin: &str,
    request_method: &str,
    request_headers: &str,
) -> axum::response::Response {
    let req = Request::builder()
        .method("OPTIONS")
        .uri(uri)
        .header("origin", origin)
        .header("access-control-request-method", request_method)
        .header("access-control-request-headers", request_headers)
        .body(Body::empty())
        .unwrap();
    router.oneshot(req).await.unwrap()
}

/// Helper: send an `OPTIONS` preflight and return `(status, allow_origin_header, headers)`.
async fn preflight_headers(
    router: Router,
    uri: &str,
    origin: &str,
) -> (StatusCode, Option<String>, axum::http::HeaderMap) {
    let resp = preflight(router, uri, origin, "POST", "Authorization, Content-Type").await;
    let status = resp.status();
    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let headers = resp.headers().clone();
    (status, allow_origin, headers)
}

// ---------------------------------------------------------------------------
// Helpers for header assertions
// ---------------------------------------------------------------------------

fn has_header(headers: &axum::http::HeaderMap, name: &str) -> bool {
    headers.contains_key(name)
}

fn header_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Bearer layer tests — /v1/*
// ---------------------------------------------------------------------------

/// Test 1: Bearer layer allows an arbitrary cross-origin (claude.ai).
/// Expects: 200/204; `Access-Control-Allow-Origin: *`; no credentials header;
/// `Vary: Origin` is present (tower-http emits this for AllowOrigin::any()).
#[tokio::test]
async fn bearer_allowed_claude_ai() {
    let state = test_app_state().await;
    let router = bearer_router(state);
    let (status, allow_origin, headers) =
        preflight_headers(router, "/v1/chat/completions", "https://claude.ai").await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "preflight should succeed, got {status}"
    );
    assert_eq!(
        allow_origin.as_deref(),
        Some("*"),
        "bearer layer should emit wildcard allow-origin"
    );
    // credentials MUST be absent (incompatible with wildcard origin)
    assert!(
        !has_header(&headers, "access-control-allow-credentials"),
        "bearer layer must not set allow-credentials"
    );
    // tower-http emits Vary: Origin for AllowOrigin::any()
    assert!(
        has_header(&headers, "vary"),
        "Vary header should be present"
    );
}

/// Test 2: Bearer layer allows `api_external_url()` as origin.
#[tokio::test]
async fn bearer_allowed_api_hostname() {
    let state = test_app_state().await;
    let api_origin = state.config.api_external_url(); // "http://api.test.local"
    let router = bearer_router(state);
    let (status, allow_origin, _) =
        preflight_headers(router, "/v1/chat/completions", &api_origin).await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "preflight should succeed, got {status}"
    );
    assert_eq!(
        allow_origin.as_deref(),
        Some("*"),
        "bearer layer should emit wildcard allow-origin for known host"
    );
}

/// Test 3: Bearer layer allows localhost dev origin.
#[tokio::test]
async fn bearer_allowed_localhost() {
    let state = test_app_state().await;
    let router = bearer_router(state);
    let (status, allow_origin, _) =
        preflight_headers(router, "/v1/chat/completions", "http://localhost:8080").await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "preflight should succeed, got {status}"
    );
    assert_eq!(allow_origin.as_deref(), Some("*"));
}

/// Test 4: Bearer layer allows `null` origin (file:// or sandboxed iframe).
#[tokio::test]
async fn bearer_allowed_null_origin() {
    let state = test_app_state().await;
    let router = bearer_router(state);
    let (status, allow_origin, _) = preflight_headers(router, "/v1/chat/completions", "null").await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "preflight should succeed for null origin, got {status}"
    );
    assert_eq!(allow_origin.as_deref(), Some("*"));
}

/// Test 5: Anthropic endpoint (`/v1/messages`) uses the same bearer CORS policy.
#[tokio::test]
async fn bearer_anthropic_endpoint() {
    let state = test_app_state().await;
    let router = bearer_router(state);
    let (status, allow_origin, headers) =
        preflight_headers(router, "/v1/messages", "https://claude.ai").await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "Anthropic preflight should succeed, got {status}"
    );
    assert_eq!(allow_origin.as_deref(), Some("*"));
    assert!(
        !has_header(&headers, "access-control-allow-credentials"),
        "no credentials on Anthropic endpoint"
    );
}

/// Test 6: OPTIONS preflight on `/v1/chat/completions` succeeds **without** an
/// Authorization header. This is the critical regression: the bearer auth
/// middleware sits *inside* the CORS layer, so preflights are short-circuited
/// before auth runs.
#[tokio::test]
async fn bearer_preflight_no_auth_required() {
    let state = test_app_state().await;
    let router = bearer_router(state);

    // Preflight with no Authorization header at all
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/v1/chat/completions")
        .header("origin", "https://claude.ai")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "Authorization, Content-Type",
        )
        .body(Body::empty())
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();

    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::NO_CONTENT,
        "OPTIONS preflight must not require auth; got {}",
        resp.status()
    );
    let allow_origin = header_value(resp.headers(), "access-control-allow-origin");
    assert_eq!(
        allow_origin.as_deref(),
        Some("*"),
        "CORS header must be present even without auth"
    );
}

/// Test 7: An actual POST request (no token) returns 401, but CORS headers
/// are still present on the error response. tower-http attaches CORS headers
/// to non-2xx responses too.
#[tokio::test]
async fn bearer_actual_request_cors_headers_on_error() {
    let state = test_app_state().await;
    let router = bearer_router(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("origin", "https://claude.ai")
        .header("content-type", "application/json")
        .body(Body::from("{\"model\":\"x\",\"messages\":[]}"))
        .unwrap();

    let resp = router.oneshot(req).await.unwrap();

    // Expect 401 from bearer_auth_middleware
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // CORS header must still be present
    let allow_origin = header_value(resp.headers(), "access-control-allow-origin");
    assert_eq!(
        allow_origin.as_deref(),
        Some("*"),
        "CORS header must be present on 401 response"
    );
}

// ---------------------------------------------------------------------------
// Strict layer tests — /api/* and /auth/*
// ---------------------------------------------------------------------------

/// Test 8: Strict layer allows `api_external_url()` origin with credentials.
#[tokio::test]
async fn strict_allowed_api_hostname() {
    let state = test_app_state().await;
    let api_origin = state.config.api_external_url(); // "http://api.test.local"
    let router = strict_router(state);

    let (status, allow_origin, headers) =
        preflight_headers(router, "/api/user/tokens", &api_origin).await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "strict preflight for api origin should succeed, got {status}"
    );
    assert_eq!(
        allow_origin.as_deref(),
        Some(api_origin.as_str()),
        "strict layer should echo the matched origin (not wildcard)"
    );
    assert_eq!(
        header_value(&headers, "access-control-allow-credentials").as_deref(),
        Some("true"),
        "strict layer must set allow-credentials: true"
    );
    assert!(
        has_header(&headers, "vary"),
        "Vary header should be present"
    );
}

/// Test 9: Strict layer allows `chat_external_url()` origin with credentials.
#[tokio::test]
async fn strict_allowed_chat_hostname() {
    let state = test_app_state().await;
    let chat_origin = state.config.chat_external_url(); // "http://chat.test.local"
    let router = strict_router(state);

    let (status, allow_origin, headers) =
        preflight_headers(router, "/api/user/tokens", &chat_origin).await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "strict preflight for chat origin should succeed, got {status}"
    );
    assert_eq!(
        allow_origin.as_deref(),
        Some(chat_origin.as_str()),
        "strict layer should echo chat origin"
    );
    assert_eq!(
        header_value(&headers, "access-control-allow-credentials").as_deref(),
        Some("true"),
        "strict layer must set allow-credentials: true for chat origin"
    );
}

/// Test 10: Strict layer does NOT allow `https://claude.ai`.
/// tower-http 0.6 omits `Access-Control-Allow-Origin` on no-match.
/// Note (experimentally verified): tower-http 0.6 still emits
/// `Access-Control-Allow-Credentials` unconditionally when `allow_credentials(true)`
/// is set, even on a no-match. Browsers correctly block the request when
/// `Access-Control-Allow-Origin` is absent regardless. We assert only the
/// security-relevant property: no allow-origin emitted.
#[tokio::test]
async fn strict_disallowed_claude_ai() {
    let state = test_app_state().await;
    let router = strict_router(state);

    let (_, allow_origin, _headers) =
        preflight_headers(router, "/api/user/tokens", "https://claude.ai").await;

    // The critical property: no allow-origin → browser will block
    assert!(
        allow_origin.is_none(),
        "strict layer must not emit allow-origin for disallowed origin, got {allow_origin:?}"
    );
    // Note: tower-http 0.6 emits access-control-allow-credentials even on no-match
    // when allow_credentials(true) is configured. The browser still blocks the
    // request because Access-Control-Allow-Origin is absent. This is a tower-http
    // implementation detail we document but do not assert (it's harmless).
}

/// Test 11: Strict layer does NOT allow `null` origin.
#[tokio::test]
async fn strict_disallowed_null_origin() {
    let state = test_app_state().await;
    let router = strict_router(state);

    let (_, allow_origin, _) = preflight_headers(router, "/api/user/tokens", "null").await;

    assert!(
        allow_origin.is_none(),
        "strict layer must not match null origin, got {allow_origin:?}"
    );
}

/// Test 12: Auth routes (`/auth/providers`) accept strict-allowed origin.
#[tokio::test]
async fn strict_auth_routes_positive() {
    let state = test_app_state().await;
    let api_origin = state.config.api_external_url();
    let router = strict_router(state);

    let (status, allow_origin, headers) =
        preflight_headers(router, "/auth/providers", &api_origin).await;

    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "auth route preflight should succeed, got {status}"
    );
    assert_eq!(
        allow_origin.as_deref(),
        Some(api_origin.as_str()),
        "strict layer should match api origin on /auth route"
    );
    assert_eq!(
        header_value(&headers, "access-control-allow-credentials").as_deref(),
        Some("true")
    );
}

/// Test 13: Auth routes block a disallowed (evil) origin.
#[tokio::test]
async fn strict_auth_routes_negative() {
    let state = test_app_state().await;
    let router = strict_router(state);

    let (_, allow_origin, _) =
        preflight_headers(router, "/auth/providers", "https://evil.example.com").await;

    assert!(
        allow_origin.is_none(),
        "strict layer must block evil origin on /auth route, got {allow_origin:?}"
    );
}

// ---------------------------------------------------------------------------
// Layer isolation tests
// ---------------------------------------------------------------------------

/// Test 14: `/v1/*` does NOT set `Access-Control-Allow-Credentials`, even when
/// the origin is one that the strict layer would allow. Catches double-wrapping.
#[tokio::test]
async fn isolation_v1_no_credentials() {
    let state = test_app_state().await;
    let api_origin = state.config.api_external_url();
    let router = bearer_router(state);

    let (_, _, headers) = preflight_headers(router, "/v1/chat/completions", &api_origin).await;

    assert!(
        !has_header(&headers, "access-control-allow-credentials"),
        "bearer layer must never emit allow-credentials, even for strict-allowed origins"
    );
    // And it should still emit wildcard (not the echoed origin)
    let allow_origin = header_value(&headers, "access-control-allow-origin");
    assert_eq!(
        allow_origin.as_deref(),
        Some("*"),
        "bearer layer emits * not echoed origin"
    );
}

/// Test 15: `/api/*` with an arbitrary origin gets no `Access-Control-Allow-Origin`.
/// The bearer layer's `Any` policy must NOT leak into the strict-only routes.
#[tokio::test]
async fn isolation_api_no_wildcard() {
    let state = test_app_state().await;
    let router = strict_router(state);

    let (_, allow_origin, _) =
        preflight_headers(router, "/api/user/tokens", "https://random.example.com").await;

    assert!(
        allow_origin.is_none(),
        "strict-only /api/* must not emit allow-origin for random origin; got {allow_origin:?}"
    );
}
