//! Tests for meta token functionality (per-user Open WebUI usage attribution).
//!
//! Meta tokens are bookkeeping tokens that attribute Open WebUI usage to
//! individual users instead of the shared internal (bootstrap admin) token.
//!
//! # Test groups
//!
//! ## 1. ensure_meta_token
//! - **ensure_meta_token_creates_new** — creates a meta=1 token row
//! - **ensure_meta_token_idempotent** — two calls return the same token ID
//!
//! ## 2. resolve_meta_user
//! - **resolve_meta_user_found** — email lookup + meta token provisioning
//! - **resolve_meta_user_not_found** — unknown email returns None
//!
//! ## 3. Token list filtering
//! - **meta_token_hidden_from_list** — meta tokens excluded from user token list
//!
//! ## 4. Usage attribution via OpenAI endpoint
//! - **meta_usage_attribution** — internal token + `user` field → usage logged under that user
//! - **meta_fallback_unknown_email** — unknown email → fallback to bootstrap admin
//! - **meta_no_override_for_regular_tokens** — `user` field ignored for non-internal tokens

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use crate::api::{openai, user};
use crate::auth::tokens::{self, hash_token};
use crate::auth::{self, SessionAuth};
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
        model_path: "/tmp/test-models".to_string(),
        model_host_path: "/tmp/test-models".to_string(),
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

/// Insert an IDP + user + token into the test DB and return the plaintext token.
async fn create_test_token(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    user_id: &str,
    internal: bool,
) -> String {
    ensure_test_user(pool, user_id).await;

    let plaintext = format!("se-{}", uuid::Uuid::new_v4());
    let token_hash = hash_token(&plaintext);
    let token_id = uuid::Uuid::new_v4().to_string();

    sqlx::query(
        "INSERT INTO tokens (id, user_id, name, token_hash, internal) \
         VALUES (?, ?, 'test', ?, ?)",
    )
    .bind(&token_id)
    .bind(user_id)
    .bind(&token_hash)
    .bind(internal)
    .execute(pool)
    .await
    .unwrap();

    plaintext
}

/// Insert a model row marked as loaded, a container_secrets row, and register
/// a concurrency gate slot.
async fn insert_test_model(state: &AppState, model_id: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO models (id, hf_repo, loaded, backend_type) \
         VALUES (?, ?, 1, 'llamacpp')",
    )
    .bind(model_id)
    .bind(model_id)
    .execute(&state.db.pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT OR IGNORE INTO container_secrets (model_id, container_uid, api_key) \
         VALUES (?, 1000, 'test-api-key')",
    )
    .bind(model_id)
    .execute(&state.db.pool)
    .await
    .unwrap();

    state.scheduler.gate().register(model_id, 1).await;
}

/// Build a `/v1/*` router with real bearer_auth_middleware.
fn openai_router(state: Arc<AppState>) -> Router {
    let openai_routes = openai::routes(state.clone()).layer(middleware::from_fn_with_state(
        state.clone(),
        auth::bearer_auth_middleware,
    ));
    Router::new().nest("/v1", openai_routes)
}

/// Build a `/user/*` router with fake session auth for the given user.
fn user_api_router(state: Arc<AppState>, user_id: &str) -> Router {
    let user_id = user_id.to_string();
    let auth_layer = middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| {
            let user_id = user_id.clone();
            async move {
                req.extensions_mut().insert(SessionAuth {
                    user_id,
                    is_admin: false,
                    email: None,
                    display_name: None,
                });
                Ok::<_, std::convert::Infallible>(next.run(req).await)
            }
        },
    );

    Router::new()
        .nest("/user", user::routes(state))
        .layer(auth_layer)
}

async fn bearer_post(router: &Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", token))
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn json_get(router: &Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
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

// ---------------------------------------------------------------------------
// 1. ensure_meta_token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ensure_meta_token_creates_new() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "alice").await;

    let token_id = tokens::ensure_meta_token(&state.db, "alice").await.unwrap();

    // Verify row in DB
    let row: (i64, String, i64) =
        sqlx::query_as("SELECT meta, name, revoked FROM tokens WHERE id = ?")
            .bind(&token_id)
            .fetch_one(&state.db.pool)
            .await
            .unwrap();

    assert_eq!(row.0, 1, "meta should be 1");
    assert_eq!(row.1, "Open WebUI");
    assert_eq!(row.2, 0, "should not be revoked");
}

#[tokio::test]
async fn ensure_meta_token_idempotent() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "alice").await;

    let id1 = tokens::ensure_meta_token(&state.db, "alice").await.unwrap();
    let id2 = tokens::ensure_meta_token(&state.db, "alice").await.unwrap();

    assert_eq!(id1, id2, "should return the same token ID on second call");

    // Verify only one meta token row
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM tokens WHERE user_id = 'alice' AND meta = 1")
            .fetch_one(&state.db.pool)
            .await
            .unwrap();
    assert_eq!(count.0, 1);
}

// ---------------------------------------------------------------------------
// 2. resolve_meta_user
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_meta_user_found() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "alice").await;

    let result = tokens::resolve_meta_user(&state.db, "alice@test.com")
        .await
        .unwrap();

    let meta = result.expect("should resolve alice");
    assert_eq!(meta.user_id, "alice");
    assert!(!meta.token_id.is_empty());
}

#[tokio::test]
async fn resolve_meta_user_not_found() {
    let state = test_app_state().await;

    let result = tokens::resolve_meta_user(&state.db, "nobody@example.com")
        .await
        .unwrap();

    assert!(result.is_none(), "unknown email should return None");
}

// ---------------------------------------------------------------------------
// 3. Token list filtering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn meta_token_hidden_from_list() {
    let state = test_app_state().await;
    ensure_test_user(&state.db.pool, "alice").await;

    // Create a regular token
    let _regular = tokens::create_token(&state.db, "alice", "My Token", None, None, None)
        .await
        .unwrap();

    // Create a meta token
    let _meta_id = tokens::ensure_meta_token(&state.db, "alice").await.unwrap();

    // Query the user token list endpoint
    let router = user_api_router(state, "alice");
    let (status, body) = json_get(&router, "/user/tokens").await;

    assert_eq!(status, StatusCode::OK);
    let token_list = body["tokens"].as_array().unwrap();

    // Should only see the regular token, not the meta token
    assert_eq!(token_list.len(), 1, "meta token should be hidden");
    assert_eq!(token_list[0]["name"], "My Token");
}

// ---------------------------------------------------------------------------
// 4. Usage attribution via OpenAI endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn meta_usage_attribution() {
    let state = test_app_state().await;
    // Internal token owned by bootstrap admin
    let internal_token = create_test_token(&state.db.pool, "bootstrap", true).await;
    // Alice is a regular user whose email will appear in the `user` field
    ensure_test_user(&state.db.pool, "alice").await;
    insert_test_model(&state, "test-model").await;
    let router = openai_router(state.clone());

    // Send request with `user` field set to alice's email
    let (_status, _body) = bearer_post(
        &router,
        "/v1/chat/completions",
        &internal_token,
        serde_json::json!({
            "model": "test-model",
            "messages": [],
            "user": "alice@test.com"
        }),
    )
    .await;

    // Give the background usage logging task a moment to complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Check that usage was logged under alice (not bootstrap)
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT user_id, token_id FROM usage_log ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(&state.db.pool)
            .await
            .unwrap();

    let (user_id, token_id) = row.expect("usage_log should have an entry");
    assert_eq!(user_id, "alice", "usage should be attributed to alice");

    // Verify the token used is a meta token
    let meta_flag: (i64,) = sqlx::query_as("SELECT meta FROM tokens WHERE id = ?")
        .bind(&token_id)
        .fetch_one(&state.db.pool)
        .await
        .unwrap();
    assert_eq!(meta_flag.0, 1, "token should be a meta token");
}

#[tokio::test]
async fn meta_fallback_unknown_email() {
    let state = test_app_state().await;
    let internal_token = create_test_token(&state.db.pool, "bootstrap", true).await;
    insert_test_model(&state, "test-model").await;
    let router = openai_router(state.clone());

    let (_status, _body) = bearer_post(
        &router,
        "/v1/chat/completions",
        &internal_token,
        serde_json::json!({
            "model": "test-model",
            "messages": [],
            "user": "nobody@unknown.com"
        }),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let row: Option<(String,)> =
        sqlx::query_as("SELECT user_id FROM usage_log ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(&state.db.pool)
            .await
            .unwrap();

    let (user_id,) = row.expect("usage_log should have an entry");
    assert_eq!(
        user_id, "bootstrap",
        "unknown email should fall back to bootstrap admin"
    );
}

#[tokio::test]
async fn meta_no_override_for_regular_tokens() {
    let state = test_app_state().await;
    // Regular (non-internal) token for bob
    let regular_token = create_test_token(&state.db.pool, "bob", false).await;
    ensure_test_user(&state.db.pool, "alice").await;
    insert_test_model(&state, "test-model").await;
    let router = openai_router(state.clone());

    // Even though `user` is set to alice's email, bob's regular token should NOT
    // trigger meta resolution — usage should be logged under bob.
    let (_status, _body) = bearer_post(
        &router,
        "/v1/chat/completions",
        &regular_token,
        serde_json::json!({
            "model": "test-model",
            "messages": [],
            "user": "alice@test.com"
        }),
    )
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let row: Option<(String,)> =
        sqlx::query_as("SELECT user_id FROM usage_log ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(&state.db.pool)
            .await
            .unwrap();

    let (user_id,) = row.expect("usage_log should have an entry");
    assert_eq!(
        user_id, "bob",
        "regular tokens should not use meta resolution"
    );
}
