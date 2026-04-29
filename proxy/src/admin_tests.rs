//! Tests for admin endpoints.
//!
//! # Test groups
//!
//! ## delete_model — DELETE /api/admin/models/{id}
//!
//! - **delete_model_missing_returns_404** — unknown id → 404.
//! - **delete_model_happy_path_returns_200** — no pins, no files on disk → 200.
//! - **delete_model_stale_pin_nulled_and_succeeds** — revoked+soft-deleted
//!   token pinning the model → 200, pin nulled.
//! - **delete_model_blocked_by_active_pin_returns_409** — active token pin
//!   blocks deletion, 409 with `blocking_tokens` list, no side effects.
//! - **delete_model_override_soft_deletes_tokens_and_succeeds** — same setup
//!   with `?override=true` → 200, token soft-deleted (revoked+deleted_at),
//!   pin nulled, model row gone.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use crate::api::admin;
use crate::auth::SessionAuth;
use crate::config::AppConfig;
use crate::db::Database;
use crate::docker::DockerManager;
use crate::metrics::MetricsBroadcaster;
use crate::scheduler::reservation::ReservationBroadcaster;
use crate::scheduler::Scheduler;
use crate::AppState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
        model_path: "/tmp/test-models-admin-tests".to_string(),
        model_host_path: "/tmp/test-models-admin-tests".to_string(),
        ui_path: "/tmp/test-ui".to_string(),
        api_hostname: "localhost".to_string(),
        chat_hostname: "localhost".to_string(),
        cookie_domain: None,
        backend_network: "test-network".to_string(),
        acme_contact: None,
        acme_staging: false,
        webui_backend_url: "http://localhost:8080".to_string(),
        webui_api_key: None,
        queue_timeout_secs: 30,
        secure_cookies: false,
        db_encryption_key: None,
        db_encryption_key_old: None,
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

/// Build an admin-scoped router with a fake session-auth middleware that
/// injects a `SessionAuth` carrying admin privileges.
fn admin_router(state: Arc<AppState>, user_id: &str) -> Router {
    let user_id = user_id.to_string();
    let auth_layer = middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| {
            let user_id = user_id.clone();
            async move {
                req.extensions_mut().insert(SessionAuth {
                    user_id,
                    is_admin: true,
                    email: None,
                    display_name: None,
                });
                Ok::<_, std::convert::Infallible>(next.run(req).await)
            }
        },
    );

    Router::new()
        .nest("/admin", admin::routes(state))
        .layer(auth_layer)
}

async fn ensure_test_user(pool: &sqlx::Pool<sqlx::Sqlite>, user_id: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO idp_configs (id, name, issuer, client_id, client_secret_enc) \
         VALUES ('test-idp', 'test', 'https://test', 'client', 'secret')",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT OR IGNORE INTO users (id, idp_id, subject, email) \
         VALUES (?, 'test-idp', ?, ?)",
    )
    .bind(user_id)
    .bind(user_id)
    .bind(format!("{}@test.com", user_id))
    .execute(pool)
    .await
    .unwrap();
}

/// Insert a model row with `loaded = 0` so the DELETE handler skips the
/// Docker stop path. The backing filesystem directory is intentionally not
/// created — the handler's `.exists()` guard will skip the `remove_dir_all`
/// call, keeping the tests hermetic.
async fn insert_model(pool: &sqlx::Pool<sqlx::Sqlite>, id: &str, hf_repo: &str) {
    sqlx::query(
        "INSERT INTO models (id, hf_repo, loaded, backend_type) \
         VALUES (?, ?, 0, 'llamacpp')",
    )
    .bind(id)
    .bind(hf_repo)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert a token with `specific_model_id` set. `revoked` and `deleted_at`
/// control whether the pin is considered active or stale.
async fn insert_pinned_token(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    token_id: &str,
    user_id: &str,
    model_id: &str,
    name: &str,
    revoked: bool,
    soft_deleted: bool,
) {
    ensure_test_user(pool, user_id).await;

    // token_hash must be unique; derive from token_id for test purposes.
    let token_hash = format!("hash-{}", token_id);
    sqlx::query(
        "INSERT INTO tokens (id, user_id, name, token_hash, specific_model_id, revoked, deleted_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(token_id)
    .bind(user_id)
    .bind(name)
    .bind(&token_hash)
    .bind(model_id)
    .bind(if revoked { 1 } else { 0 })
    .bind(if soft_deleted {
        Some("2026-01-01T00:00:00")
    } else {
        None
    })
    .execute(pool)
    .await
    .unwrap();
}

async fn json_delete(router: &Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

async fn model_exists(pool: &sqlx::Pool<sqlx::Sqlite>, id: &str) -> bool {
    let row: Option<(String,)> = sqlx::query_as("SELECT id FROM models WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap();
    row.is_some()
}

/// Load a token's mutable state for assertions.
async fn get_token_state(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    token_id: &str,
) -> (Option<String>, i64, Option<String>) {
    sqlx::query_as::<_, (Option<String>, i64, Option<String>)>(
        "SELECT specific_model_id, revoked, deleted_at FROM tokens WHERE id = ?",
    )
    .bind(token_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_model_missing_returns_404() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "admin1").await;
    let router = admin_router(state, "admin1");

    let (status, _body) = json_delete(&router, "/admin/models/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_model_happy_path_returns_200() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "admin1").await;
    insert_model(&state.db.pool, "model-happy", "owner/happy-GGUF").await;

    let router = admin_router(state.clone(), "admin1");
    let (status, _body) = json_delete(&router, "/admin/models/model-happy").await;

    assert_eq!(status, StatusCode::OK);
    assert!(!model_exists(&state.db.pool, "model-happy").await);
}

#[tokio::test]
async fn delete_model_stale_pin_nulled_and_succeeds() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "admin1").await;
    insert_model(&state.db.pool, "model-stale", "owner/stale-GGUF").await;
    // Token is both revoked and soft-deleted — stale from the user's point of
    // view. Its pin should be nulled silently so the model delete can proceed.
    insert_pinned_token(
        &state.db.pool,
        "tok-stale",
        "user1",
        "model-stale",
        "old-token",
        /* revoked */ true,
        /* soft_deleted */ true,
    )
    .await;

    let router = admin_router(state.clone(), "admin1");
    let (status, _body) = json_delete(&router, "/admin/models/model-stale").await;

    assert_eq!(status, StatusCode::OK);
    assert!(!model_exists(&state.db.pool, "model-stale").await);

    let (specific, revoked, deleted_at) = get_token_state(&state.db.pool, "tok-stale").await;
    assert_eq!(specific, None, "pin should be nulled");
    assert_eq!(revoked, 1, "revoked flag preserved");
    assert!(deleted_at.is_some(), "deleted_at preserved");
}

#[tokio::test]
async fn delete_model_blocked_by_active_pin_returns_409() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "admin1").await;
    insert_model(&state.db.pool, "model-pinned", "owner/pinned-GGUF").await;
    insert_pinned_token(
        &state.db.pool,
        "tok-active",
        "user1",
        "model-pinned",
        "live-token",
        /* revoked */ false,
        /* soft_deleted */ false,
    )
    .await;

    let router = admin_router(state.clone(), "admin1");
    let (status, body) = json_delete(&router, "/admin/models/model-pinned").await;

    assert_eq!(status, StatusCode::CONFLICT);

    // Body should advertise the blocking tokens so the UI can render them.
    let blockers = body
        .get("blocking_tokens")
        .and_then(|v| v.as_array())
        .expect("blocking_tokens array in response");
    assert_eq!(blockers.len(), 1);
    let blocker = &blockers[0];
    assert_eq!(
        blocker.get("id").and_then(|v| v.as_str()),
        Some("tok-active")
    );
    assert_eq!(
        blocker.get("name").and_then(|v| v.as_str()),
        Some("live-token")
    );

    // Nothing should have been mutated.
    assert!(model_exists(&state.db.pool, "model-pinned").await);
    let (specific, revoked, deleted_at) = get_token_state(&state.db.pool, "tok-active").await;
    assert_eq!(specific.as_deref(), Some("model-pinned"));
    assert_eq!(revoked, 0);
    assert!(deleted_at.is_none());
}

#[tokio::test]
async fn delete_model_override_soft_deletes_tokens_and_succeeds() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "admin1").await;
    insert_model(&state.db.pool, "model-override", "owner/override-GGUF").await;
    insert_pinned_token(
        &state.db.pool,
        "tok-override",
        "user1",
        "model-override",
        "live-token",
        /* revoked */ false,
        /* soft_deleted */ false,
    )
    .await;

    let router = admin_router(state.clone(), "admin1");
    let (status, _body) = json_delete(&router, "/admin/models/model-override?override=true").await;

    assert_eq!(status, StatusCode::OK);
    assert!(!model_exists(&state.db.pool, "model-override").await);

    // Token row is preserved for audit but now soft-deleted + unpinned.
    let (specific, revoked, deleted_at) = get_token_state(&state.db.pool, "tok-override").await;
    assert_eq!(specific, None, "pin should be nulled");
    assert_eq!(revoked, 1, "token should be revoked");
    assert!(deleted_at.is_some(), "token should be soft-deleted");
}
