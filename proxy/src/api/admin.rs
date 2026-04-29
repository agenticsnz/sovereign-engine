use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use uuid::Uuid;

use super::common;
use super::error;
use crate::auth::SessionAuth;
use crate::db::models::{IdpConfigPublic, User};
use crate::docker::runtime_overrides::ModelRuntimeOverrides;
use crate::AppState;

/// Row from `models` used by the estimate_vram handler.
#[derive(sqlx::FromRow)]
struct ModelMetadataRow {
    size_bytes: i64,
    context_length: Option<i64>,
    n_layers: Option<i64>,
    n_heads: Option<i64>,
    n_kv_heads: Option<i64>,
    embedding_length: Option<i64>,
    key_length: Option<i64>,
    value_length: Option<i64>,
    sliding_window: Option<i64>,
    kv_bytes_per_token_global: Option<i64>,
    kv_bytes_per_token_swa: Option<i64>,
}

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        // IdP management
        .route("/idps", get(list_idps).post(create_idp))
        .route("/idps/{id}", put(update_idp).delete(disable_idp))
        // Category management
        .route("/categories", get(list_categories).post(create_category))
        .route(
            "/categories/{id}",
            put(update_category).delete(delete_category),
        )
        // Model management
        .route("/models", get(list_models))
        .route("/models/register", post(register_model))
        .route("/models/{id}", put(update_model).delete(delete_model))
        // Phase 7: backend crash diagnostics
        .route("/models/{id}/crash_history", get(get_crash_history))
        .route("/models/{id}/crash_log/{occurred_at}", get(get_crash_log))
        // User management
        .route("/users", get(list_users))
        .route("/users/{id}", put(update_user))
        // System status
        .route("/system", get(system_status))
        // Containers
        .route("/containers", get(list_containers))
        .route("/containers/start", post(start_container))
        .route("/containers/stop", post(stop_container))
        .route("/containers/estimate", post(estimate_vram))
        // Settings
        .route("/settings", get(get_settings).put(update_settings))
        // Usage analytics
        .route("/usage", get(admin_usage))
        .route("/usage/timeline", get(admin_usage_timeline))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// IdP Management
// ---------------------------------------------------------------------------

/// GET /api/admin/idps — List all IdP configs.
async fn list_idps(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match sqlx::query_as::<_, IdpConfigPublic>(
        "SELECT id, name, issuer, client_id, scopes, enabled, created_at FROM idp_configs",
    )
    .fetch_all(&state.db.pool)
    .await
    {
        Ok(idps) => Json(serde_json::json!({ "idps": idps })).into_response(),
        Err(e) => error::internal_error("list_idps", e),
    }
}

#[derive(Debug, Deserialize)]
struct CreateIdpRequest {
    name: String,
    issuer: String,
    client_id: String,
    client_secret: String,
    scopes: Option<String>,
}

/// POST /api/admin/idps — Create a new IdP.
async fn create_idp(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Json(req): Json<CreateIdpRequest>,
) -> impl IntoResponse {
    if let Some(r) = error::validate_len("name", &req.name, error::MAX_NAME)
        .or_else(|| error::validate_len("issuer", &req.issuer, error::MAX_URL))
        .or_else(|| error::validate_len("client_id", &req.client_id, error::MAX_NAME))
        .or_else(|| error::validate_len("client_secret", &req.client_secret, error::MAX_SECRET))
    {
        return r;
    }
    let id = Uuid::new_v4().to_string();
    let scopes = req
        .scopes
        .unwrap_or_else(|| "openid email profile".to_string());
    let client_secret_enc = match &state.config.db_encryption_key {
        Some(key) => match crate::db::crypto::encrypt(&req.client_secret, key) {
            Ok(enc) => enc,
            Err(e) => return error::internal_error("create_idp:encrypt", e),
        },
        None => req.client_secret.clone(),
    };

    match sqlx::query(
        "INSERT INTO idp_configs (id, name, issuer, client_id, client_secret_enc, scopes) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&req.name)
    .bind(&req.issuer)
    .bind(&req.client_id)
    .bind(&client_secret_enc)
    .bind(&scopes)
    .execute(&state.db.pool)
    .await
    {
        Ok(_) => {
            info!(target: "audit", action = "idp.create", actor = %session.user_id, resource = %id, name = %req.name, "Admin created IdP");
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "id": id, "name": req.name })),
            )
                .into_response()
        }
        Err(e) => error::api_error(StatusCode::BAD_REQUEST, "create_idp", e),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateIdpRequest {
    name: Option<String>,
    issuer: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    scopes: Option<String>,
    enabled: Option<bool>,
}

/// PUT /api/admin/idps/:id — Update an IdP configuration.
async fn update_idp(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Path(id): Path<String>,
    Json(req): Json<UpdateIdpRequest>,
) -> impl IntoResponse {
    if let Some(r) = req
        .name
        .as_deref()
        .and_then(|v| error::validate_len("name", v, error::MAX_NAME))
        .or_else(|| {
            req.issuer
                .as_deref()
                .and_then(|v| error::validate_len("issuer", v, error::MAX_URL))
        })
        .or_else(|| {
            req.client_id
                .as_deref()
                .and_then(|v| error::validate_len("client_id", v, error::MAX_NAME))
        })
        .or_else(|| {
            req.client_secret
                .as_deref()
                .and_then(|v| error::validate_len("client_secret", v, error::MAX_SECRET))
        })
    {
        return r;
    }
    // Check that the IdP exists
    let exists: Option<(String,)> = match sqlx::query_as("SELECT id FROM idp_configs WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db.pool)
        .await
    {
        Ok(row) => row,
        Err(e) => {
            return error::internal_error("update_idp:lookup", e);
        }
    };

    if exists.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "IdP not found" })),
        )
            .into_response();
    }

    // Build dynamic update query
    let mut sets = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    let mut enabled_val: Option<i32> = None;

    if let Some(ref name) = req.name {
        sets.push("name = ?");
        binds.push(name.clone());
    }
    if let Some(ref issuer) = req.issuer {
        sets.push("issuer = ?");
        binds.push(issuer.clone());
    }
    if let Some(ref client_id) = req.client_id {
        sets.push("client_id = ?");
        binds.push(client_id.clone());
    }
    if let Some(ref client_secret) = req.client_secret {
        sets.push("client_secret_enc = ?");
        let secret_val = match &state.config.db_encryption_key {
            Some(key) => match crate::db::crypto::encrypt(client_secret, key) {
                Ok(enc) => enc,
                Err(e) => return error::internal_error("update_idp:encrypt", e),
            },
            None => client_secret.clone(),
        };
        binds.push(secret_val);
    }
    if let Some(ref scopes) = req.scopes {
        sets.push("scopes = ?");
        binds.push(scopes.clone());
    }
    if let Some(enabled) = req.enabled {
        sets.push("enabled = ?");
        enabled_val = Some(if enabled { 1 } else { 0 });
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No fields to update" })),
        )
            .into_response();
    }

    let sql = format!("UPDATE idp_configs SET {} WHERE id = ?", sets.join(", "));
    let mut query = sqlx::query(&sql);
    for val in &binds {
        query = query.bind(val);
    }
    if let Some(ev) = enabled_val {
        query = query.bind(ev);
    }
    query = query.bind(&id);

    match query.execute(&state.db.pool).await {
        Ok(_) => {
            info!(target: "audit", action = "idp.update", actor = %session.user_id, resource = %id, "Admin updated IdP");
            Json(serde_json::json!({ "status": "updated" })).into_response()
        }
        Err(e) => error::internal_error("update_idp", e),
    }
}

/// DELETE /api/admin/idps/:id — Disable (soft-delete) an IdP.
async fn disable_idp(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match sqlx::query("UPDATE idp_configs SET enabled = 0 WHERE id = ?")
        .bind(&id)
        .execute(&state.db.pool)
        .await
    {
        Ok(result) => {
            if result.rows_affected() == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "IdP not found" })),
                )
                    .into_response()
            } else {
                info!(target: "audit", action = "idp.disable", actor = %session.user_id, resource = %id, "Admin disabled IdP");
                Json(serde_json::json!({ "status": "disabled" })).into_response()
            }
        }
        Err(e) => error::internal_error("disable_idp", e),
    }
}

// ---------------------------------------------------------------------------
// Category Management
// ---------------------------------------------------------------------------

/// GET /api/admin/categories — List all model categories.
async fn list_categories(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    common::fetch_all_categories(&state.db.pool).await
}

#[derive(Debug, Deserialize)]
struct CreateCategoryRequest {
    name: String,
    description: Option<String>,
    preferred_model_id: Option<String>,
}

/// POST /api/admin/categories — Create a new model category.
async fn create_category(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Json(req): Json<CreateCategoryRequest>,
) -> impl IntoResponse {
    if let Some(r) = error::validate_len("name", &req.name, error::MAX_NAME).or_else(|| {
        req.description
            .as_deref()
            .and_then(|v| error::validate_len("description", v, error::MAX_DESCRIPTION))
    }) {
        return r;
    }
    let id = Uuid::new_v4().to_string();
    let desc = req.description.unwrap_or_default();

    match sqlx::query(
        "INSERT INTO model_categories (id, name, description, preferred_model_id) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&req.name)
    .bind(&desc)
    .bind(&req.preferred_model_id)
    .execute(&state.db.pool)
    .await
    {
        Ok(_) => {
            info!(target: "audit", action = "category.create", actor = %session.user_id, resource = %id, name = %req.name, "Admin created category");
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "id": id, "name": req.name })),
            )
                .into_response()
        }
        Err(e) => error::api_error(StatusCode::BAD_REQUEST, "create_category", e),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateCategoryRequest {
    name: Option<String>,
    description: Option<String>,
    preferred_model_id: Option<String>,
}

/// PUT /api/admin/categories/:id — Update a category.
async fn update_category(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Path(id): Path<String>,
    Json(req): Json<UpdateCategoryRequest>,
) -> impl IntoResponse {
    if let Some(r) = req
        .name
        .as_deref()
        .and_then(|v| error::validate_len("name", v, error::MAX_NAME))
        .or_else(|| {
            req.description
                .as_deref()
                .and_then(|v| error::validate_len("description", v, error::MAX_DESCRIPTION))
        })
    {
        return r;
    }
    let mut sets = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref name) = req.name {
        sets.push("name = ?");
        binds.push(name.clone());
    }
    if let Some(ref description) = req.description {
        sets.push("description = ?");
        binds.push(description.clone());
    }
    if let Some(ref preferred) = req.preferred_model_id {
        sets.push("preferred_model_id = ?");
        binds.push(preferred.clone());
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "No fields to update" })),
        )
            .into_response();
    }

    let sql = format!(
        "UPDATE model_categories SET {} WHERE id = ?",
        sets.join(", ")
    );
    let mut query = sqlx::query(&sql);
    for val in &binds {
        query = query.bind(val);
    }
    query = query.bind(&id);

    match query.execute(&state.db.pool).await {
        Ok(result) => {
            if result.rows_affected() == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "Category not found" })),
                )
                    .into_response()
            } else {
                info!(target: "audit", action = "category.update", actor = %session.user_id, resource = %id, "Admin updated category");
                Json(serde_json::json!({ "status": "updated" })).into_response()
            }
        }
        Err(e) => error::internal_error("update_category", e),
    }
}

/// DELETE /api/admin/categories/:id — Delete a category.
async fn delete_category(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match sqlx::query("DELETE FROM model_categories WHERE id = ?")
        .bind(&id)
        .execute(&state.db.pool)
        .await
    {
        Ok(result) => {
            if result.rows_affected() == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "Category not found" })),
                )
                    .into_response()
            } else {
                info!(target: "audit", action = "category.delete", actor = %session.user_id, resource = %id, "Admin deleted category");
                Json(serde_json::json!({ "status": "deleted" })).into_response()
            }
        }
        Err(e) => error::internal_error("delete_category", e),
    }
}

// ---------------------------------------------------------------------------
// Model Management
// ---------------------------------------------------------------------------

/// GET /api/admin/models — List all registered models.
async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    common::fetch_all_models(&state.db.pool).await
}

#[derive(Debug, Deserialize)]
struct RegisterModelRequest {
    hf_repo: String,
    category_id: Option<String>,
    backend_type: Option<String>,
}

/// POST /api/admin/models/register — Register a new model.
///
/// Note: `runtime_overrides` is intentionally NOT accepted here. The struct
/// has no such field and serde silently drops unknown JSON keys, so callers
/// that include it get no error but no effect either. New rows always start
/// with the DB default `'{}'`; use `PUT /api/admin/models/:id` to set
/// overrides afterwards.
async fn register_model(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Json(req): Json<RegisterModelRequest>,
) -> impl IntoResponse {
    if let Some(r) = error::validate_len("hf_repo", &req.hf_repo, error::MAX_NAME) {
        return r;
    }
    if let Some(r) = error::validate_hf_repo(&req.hf_repo) {
        return r;
    }
    let id = Uuid::new_v4().to_string();

    let backend_type = req.backend_type.as_deref().unwrap_or("llamacpp");
    // runtime_overrides defaults to '{}' via the column DEFAULT — don't bind it here.
    match sqlx::query(
        "INSERT INTO models (id, hf_repo, category_id, backend_type) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&req.hf_repo)
    .bind(&req.category_id)
    .bind(backend_type)
    .execute(&state.db.pool)
    .await
    {
        Ok(_) => {
            info!(target: "audit", action = "model.register", actor = %session.user_id, resource = %id, hf_repo = %req.hf_repo, "Admin registered model");
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "id": id, "hf_repo": req.hf_repo })),
            )
                .into_response()
        }
        Err(e) => error::api_error(StatusCode::BAD_REQUEST, "register_model", e),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateModelRequest {
    category_id: Option<String>,
    /// Optional per-model llama-server CLI overrides. When `None`, only
    /// `category_id` is updated (preserves the historical PUT semantics).
    #[serde(default)]
    runtime_overrides: Option<ModelRuntimeOverrides>,
}

/// PUT /api/admin/models/:id — Update model metadata.
async fn update_model(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Path(id): Path<String>,
    Json(req): Json<UpdateModelRequest>,
) -> impl IntoResponse {
    // If the caller supplied runtime_overrides, validate + serialize before
    // we touch the DB so a bad payload comes back as a clean 400.
    let overrides_json: Option<String> = match &req.runtime_overrides {
        Some(o) => {
            if let Err(reason) = o.validate() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": reason })),
                )
                    .into_response();
            }
            match serde_json::to_string(o) {
                Ok(s) => Some(s),
                Err(e) => return error::internal_error("update_model:serialize_overrides", e),
            }
        }
        None => None,
    };

    let result = match &overrides_json {
        Some(json) => {
            sqlx::query("UPDATE models SET category_id = ?, runtime_overrides = ? WHERE id = ?")
                .bind(&req.category_id)
                .bind(json)
                .bind(&id)
                .execute(&state.db.pool)
                .await
        }
        None => {
            sqlx::query("UPDATE models SET category_id = ? WHERE id = ?")
                .bind(&req.category_id)
                .bind(&id)
                .execute(&state.db.pool)
                .await
        }
    };

    match result {
        Ok(result) => {
            if result.rows_affected() == 0 {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "Model not found" })),
                )
                    .into_response()
            } else {
                match &overrides_json {
                    Some(json) => {
                        info!(target: "audit", action = "model.update", actor = %session.user_id, resource = %id, runtime_overrides = %json, "Admin updated model");
                    }
                    None => {
                        info!(target: "audit", action = "model.update", actor = %session.user_id, resource = %id, "Admin updated model");
                    }
                }
                Json(serde_json::json!({ "status": "updated" })).into_response()
            }
        }
        Err(e) => error::internal_error("update_model", e),
    }
}

/// Query parameters for `DELETE /api/admin/models/:id`.
///
/// `override=true` opts in to force-revoking any currently-active tokens that
/// are pinned to this model (via `specific_model_id`). Without the override,
/// an active pin causes the handler to return 409 with a list of blockers.
#[derive(Deserialize, Default)]
struct DeleteModelQuery {
    #[serde(default, rename = "override")]
    override_: bool,
}

/// Row returned by the blocking-tokens pre-check.
#[derive(sqlx::FromRow, Serialize)]
struct BlockingToken {
    id: String,
    name: String,
    user_email: Option<String>,
}

/// DELETE /api/admin/models/:id — Delete a model.
///
/// Flow:
/// 1. Look up the model (404 on miss).
/// 2. Pre-check for active pins (`tokens.specific_model_id` where the token
///    is neither revoked nor soft-deleted). If any are found and
///    `override=true` is not set, return 409 with the blocker list — no side
///    effects.
/// 3. When `override=true`, soft-delete each blocking token (revoked=1,
///    deleted_at=now) with audit logging.
/// 4. Stop the backend container if loaded; run post-stop cleanup.
/// 5. In a single DB transaction: NULL any remaining `specific_model_id`
///    pins (covers just-overridden blockers and any pre-existing stale
///    pins), defensively delete the `container_secrets` row, and delete the
///    `models` row.
/// 6. Remove files from disk only after the DB commit succeeds, so a
///    failure never leaves an orphaned DB row or orphaned files on disk.
async fn delete_model(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Path(id): Path<String>,
    Query(params): Query<DeleteModelQuery>,
) -> impl IntoResponse {
    // 1. Look up the model.
    let model: Option<(String, String, bool, String)> =
        match sqlx::query_as("SELECT id, hf_repo, loaded, backend_type FROM models WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.db.pool)
            .await
        {
            Ok(row) => row,
            Err(e) => return error::internal_error("delete_model:lookup", e),
        };

    let (model_id, hf_repo, loaded, backend_type) = match model {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Model not found" })),
            )
                .into_response();
        }
    };

    // 2. Pre-check: active pins (not revoked, not soft-deleted).
    let blockers: Vec<BlockingToken> = match sqlx::query_as(
        "SELECT t.id AS id, t.name AS name, u.email AS user_email \
         FROM tokens t LEFT JOIN users u ON u.id = t.user_id \
         WHERE t.specific_model_id = ? AND t.revoked = 0 AND t.deleted_at IS NULL",
    )
    .bind(&model_id)
    .fetch_all(&state.db.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => return error::internal_error("delete_model:blockers", e),
    };

    if !blockers.is_empty() && !params.override_ {
        // 409 with blocker list — no state mutated.
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "Model is in use by {} active token(s). Revoke them or retry with override=true.",
                    blockers.len()
                ),
                "blocking_tokens": blockers,
            })),
        )
            .into_response();
    }

    // 3. Override path: soft-delete each blocker.
    for blocker in &blockers {
        if let Err(e) =
            sqlx::query("UPDATE tokens SET revoked = 1, deleted_at = datetime('now') WHERE id = ?")
                .bind(&blocker.id)
                .execute(&state.db.pool)
                .await
        {
            return error::internal_error("delete_model:soft_delete_token", e);
        }
        info!(
            target: "audit",
            action = "token.force_revoke",
            actor = %session.user_id,
            resource = %blocker.id,
            reason = "model_delete_override",
            model_id = %model_id,
            "Force-revoked token during model delete override"
        );
    }

    // 4. Stop the running container if loaded.
    if loaded {
        if let Err(e) = state.docker.stop_backend(&model_id, &backend_type).await {
            error!(model = %model_id, error = %e, "Failed to stop container during model delete");
            // Continue — container may already be gone.
        }
        common::post_stop_cleanup(&state, &model_id).await;
    }

    // 5. Transactional cleanup + DB delete.
    let mut tx = match state.db.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => return error::internal_error("delete_model:tx_begin", e),
    };

    // Null any remaining pins — the override path just soft-deleted the
    // active blockers, but pre-existing stale pins (already revoked or
    // already soft-deleted tokens) are still pointing at this model.
    if let Err(e) =
        sqlx::query("UPDATE tokens SET specific_model_id = NULL WHERE specific_model_id = ?")
            .bind(&model_id)
            .execute(&mut *tx)
            .await
    {
        return error::internal_error("delete_model:null_pins", e);
    }

    // Defensive: cover the case where `loaded` was stale and
    // `post_stop_cleanup` didn't run, so the FK from container_secrets is
    // guaranteed to be clear before the DELETE FROM models.
    if let Err(e) = sqlx::query("DELETE FROM container_secrets WHERE model_id = ?")
        .bind(&model_id)
        .execute(&mut *tx)
        .await
    {
        return error::internal_error("delete_model:container_secrets", e);
    }

    if let Err(e) = sqlx::query("DELETE FROM models WHERE id = ?")
        .bind(&model_id)
        .execute(&mut *tx)
        .await
    {
        return error::internal_error("delete_model:db", e);
    }

    if let Err(e) = tx.commit().await {
        return error::internal_error("delete_model:tx_commit", e);
    }

    // 6. Remove files from disk — only after the DB commit succeeded.
    let safe_repo = hf_repo.replace('/', "--");
    let model_dir = format!("{}/{}", state.config.model_path, safe_repo);
    if std::path::Path::new(&model_dir).exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&model_dir).await {
            error!(path = %model_dir, error = %e, "Failed to delete model files after DB delete");
            // DB row is already gone; surface a distinct error code so
            // operators can tell this from the DB-failure case above.
            return error::internal_error("delete_model:files", e);
        }
        info!(path = %model_dir, "Model files deleted");
    }

    info!(
        target: "audit",
        action = "model.delete",
        actor = %session.user_id,
        resource = %model_id,
        hf_repo = %hf_repo,
        overridden = params.override_,
        revoked_tokens = blockers.len(),
        "Admin deleted model"
    );

    Json(serde_json::json!({
        "status": "deleted",
        "revoked_tokens": blockers.len(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// User Management
// ---------------------------------------------------------------------------

/// GET /api/admin/users — List all users with usage stats.
async fn list_users(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match sqlx::query_as::<_, User>(
        "SELECT id, idp_id, subject, email, display_name, is_admin, created_at FROM users",
    )
    .fetch_all(&state.db.pool)
    .await
    {
        Ok(users) => {
            let mut data: Vec<serde_json::Value> = Vec::with_capacity(users.len());

            for user in &users {
                // Get usage summary for this user
                let usage: (i64, i64) = sqlx::query_as(
                    "SELECT COALESCE(COUNT(*), 0), COALESCE(SUM(input_tokens + output_tokens), 0) FROM usage_log WHERE user_id = ?",
                )
                .bind(&user.id)
                .fetch_one(&state.db.pool)
                .await
                .unwrap_or((0, 0));

                let mut entry = serde_json::to_value(user).unwrap_or_default();
                entry["usage_summary"] = serde_json::json!({
                    "total_requests": usage.0,
                    "total_tokens": usage.1,
                });
                data.push(entry);
            }

            Json(serde_json::json!({ "users": data })).into_response()
        }
        Err(e) => error::internal_error("list_users", e),
    }
}

#[derive(Debug, Deserialize)]
struct UpdateUserRequest {
    is_admin: Option<bool>,
}

/// PUT /api/admin/users/:id — Update user (toggle admin, etc).
async fn update_user(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Path(id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> impl IntoResponse {
    if let Some(is_admin) = req.is_admin {
        let admin_val: i32 = if is_admin { 1 } else { 0 };
        match sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?")
            .bind(admin_val)
            .bind(&id)
            .execute(&state.db.pool)
            .await
        {
            Ok(result) => {
                if result.rows_affected() == 0 {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "error": "User not found" })),
                    )
                        .into_response();
                }
            }
            Err(e) => {
                return error::internal_error("update_user", e);
            }
        }
    }

    info!(target: "audit", action = "user.update", actor = %session.user_id, resource = %id, "Admin updated user");
    Json(serde_json::json!({ "status": "updated" })).into_response()
}

// ---------------------------------------------------------------------------
// System Status
// ---------------------------------------------------------------------------

/// GET /api/admin/system — System overview.
async fn system_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Disk usage
    let disk = match super::hf::get_disk_usage(&state.config.model_path) {
        Ok(d) => serde_json::json!({
            "model_path": state.config.model_path,
            "total_bytes": d.total_bytes,
            "used_bytes": d.used_bytes,
            "free_bytes": d.free_bytes,
        }),
        Err(e) => serde_json::json!({
            "model_path": state.config.model_path,
            "error": e,
        }),
    };

    // Per-container VRAM (best-effort)
    let vram_map = state.docker.per_container_vram().await;

    // Container health — list managed containers and check their state.
    // Phase 7: enriched variant adds supervisor FSM state, quarantine flag,
    // and the most-recent crash summary for each managed container.
    let containers = match state.docker.list_managed_containers().await {
        Ok(containers) => {
            common::extract_container_statuses_enriched(&state, containers, &vram_map).await
        }
        Err(_) => vec![],
    };

    let queues = state.scheduler.get_queue_stats().await;
    let gates = state.scheduler.gate().status().await;

    // GPU detection
    let gpu = state.docker.detect_gpu().await;
    let available_backends = state.docker.available_backends().await;

    // GPU memory info — all detected GPUs
    let gpu_memory: Vec<serde_json::Value> = crate::docker::DockerManager::gpu_all_info()
        .await
        .into_iter()
        .map(|stats| {
            serde_json::json!({
                "gpu_type": stats.gpu_type,
                "device_index": stats.device_index,
                "total_mb": stats.total_mb,
                "used_mb": stats.used_mb,
                "free_mb": stats.free_mb,
                "utilization_percent": stats.utilization_percent,
            })
        })
        .collect();

    Json(serde_json::json!({
        "disk": disk,
        "containers": containers,
        "queues": queues,
        "gates": gates,
        "gpu": gpu,
        "gpu_memory": gpu_memory,
        "available_backends": available_backends,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Container Management (preserved from original)
// ---------------------------------------------------------------------------

/// GET /api/admin/containers — List managed Docker containers.
async fn list_containers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.docker.list_managed_containers().await {
        Ok(containers) => {
            let data: Vec<serde_json::Value> = containers
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "names": c.names,
                        "state": c.state.map(|s| format!("{:?}", s).to_lowercase()),
                        "status": c.status,
                        "labels": c.labels,
                    })
                })
                .collect();
            Json(serde_json::json!({ "containers": data })).into_response()
        }
        Err(e) => error::internal_error("list_containers", e),
    }
}

#[derive(Debug, Deserialize)]
struct StartContainerRequest {
    model_id: String,
    backend_type: Option<String>,
    gpu_type: Option<String>,
    gpu_layers: Option<u32>,
    parallel: Option<u32>,
}

/// POST /api/admin/containers/start — Start a backend container for a model.
async fn start_container(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Json(req): Json<StartContainerRequest>,
) -> impl IntoResponse {
    let params = common::StartContainerParams {
        model_id: req.model_id,
        backend_type: req.backend_type,
        gpu_type: req.gpu_type,
        gpu_layers: req.gpu_layers,
        parallel: req.parallel,
    };

    match common::start_container_core(&state, &params).await {
        Ok((container_name, url)) => {
            info!(target: "audit", action = "container.start", actor = %session.user_id, resource = %params.model_id, container = %container_name, "Admin started container");
            Json(serde_json::json!({
                "container": container_name,
                "url": url,
            }))
            .into_response()
        }
        Err(response) => response,
    }
}

// ---------------------------------------------------------------------------
// VRAM Estimation
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EstimateVramRequest {
    model_id: String,
    parallel: Option<u32>,
}

/// POST /api/admin/containers/estimate — Estimate VRAM usage for a model configuration.
async fn estimate_vram(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EstimateVramRequest>,
) -> impl IntoResponse {
    // Look up model metadata
    let model: Option<ModelMetadataRow> =
        match sqlx::query_as(
            "SELECT size_bytes, context_length, n_layers, n_heads, n_kv_heads, embedding_length, key_length, value_length, sliding_window, kv_bytes_per_token_global, kv_bytes_per_token_swa FROM models WHERE id = ?",
        )
        .bind(&req.model_id)
        .fetch_optional(&state.db.pool)
        .await
        {
            Ok(m) => m,
            Err(e) => {
                return error::internal_error("estimate_vram:lookup", e);
            }
        };

    let ModelMetadataRow {
        size_bytes,
        context_length,
        n_layers,
        n_heads,
        n_kv_heads,
        embedding_length,
        key_length,
        value_length,
        sliding_window,
        kv_bytes_per_token_global,
        kv_bytes_per_token_swa,
    } = match model {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "Model not found" })),
            )
                .into_response();
        }
    };

    let context_size = match context_length {
        Some(v) => v as u64,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Model has no context_length set — cannot estimate VRAM" })),
            )
                .into_response();
        }
    };
    let parallel = req.parallel.unwrap_or(1).max(1) as u64;

    // Model weights: GGUF file size ≈ GPU memory for quantized models
    let model_weights_mb = (size_bytes as u64) / (1024 * 1024);

    let kv_cache_mb = estimate_kv_cache_mb(&KvCacheParams {
        n_layers,
        n_heads,
        n_kv_heads,
        embedding_length,
        key_length,
        value_length,
        kv_bytes_per_token_global,
        kv_bytes_per_token_swa,
        sliding_window,
        context_size,
        parallel,
    });

    let overhead_mb: u64 = 200; // CUDA context + misc overhead
    let total_mb = model_weights_mb + kv_cache_mb + overhead_mb;

    // Get current GPU memory — sum across all GPUs
    let all_gpus = crate::docker::DockerManager::gpu_all_info().await;
    let gpu_total_mb: u64 = all_gpus.iter().map(|g| g.total_mb).sum();
    let gpu_used_mb: u64 = all_gpus.iter().map(|g| g.used_mb).sum();
    let gpu_free_mb: u64 = all_gpus.iter().map(|g| g.free_mb).sum();

    let fits = gpu_total_mb > 0 && total_mb <= gpu_free_mb;

    Json(serde_json::json!({
        "model_weights_mb": model_weights_mb,
        "kv_cache_mb": kv_cache_mb,
        "overhead_mb": overhead_mb,
        "total_mb": total_mb,
        "gpu_total_mb": gpu_total_mb,
        "gpu_used_mb": gpu_used_mb,
        "gpu_free_mb": gpu_free_mb,
        "fits": fits,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct StopContainerRequest {
    model_id: String,
}

/// POST /api/admin/containers/stop — Stop a backend container.
async fn stop_container(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Json(req): Json<StopContainerRequest>,
) -> impl IntoResponse {
    let backend_type = common::lookup_backend_type(&state.db.pool, &req.model_id).await;

    info!(model = %req.model_id, backend = %backend_type, "Stopping container");
    match state
        .docker
        .stop_backend(&req.model_id, &backend_type)
        .await
    {
        Ok(_) => {
            info!(target: "audit", action = "container.stop", actor = %session.user_id, resource = %req.model_id, backend = %backend_type, "Admin stopped container");
            common::post_stop_cleanup(&state, &req.model_id).await;
            Json(serde_json::json!({ "status": "stopped" })).into_response()
        }
        Err(e) => {
            error!(model = %req.model_id, backend = %backend_type, error = %e, "Failed to stop container");
            error::internal_error("stop_container", e)
        }
    }
}

// ---------------------------------------------------------------------------
// Settings Management
// ---------------------------------------------------------------------------

/// GET /api/admin/settings — Return current fairness/queue settings.
async fn get_settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let settings = state.scheduler.settings().await;
    Json(serde_json::json!({
        "fairness_base_priority": settings.base_priority,
        "fairness_wait_weight": settings.wait_weight,
        "fairness_usage_weight": settings.usage_weight,
        "fairness_usage_scale": settings.usage_scale,
        "fairness_window_minutes": settings.window_minutes,
        "queue_timeout_secs": settings.queue_timeout_secs,
    }))
    .into_response()
}

/// PUT /api/admin/settings — Partial update of fairness/queue settings.
async fn update_settings(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<SessionAuth>,
    Json(req): Json<HashMap<String, serde_json::Value>>,
) -> impl IntoResponse {
    use crate::scheduler::settings::save_setting;

    let valid_keys = [
        "fairness_base_priority",
        "fairness_wait_weight",
        "fairness_usage_weight",
        "fairness_usage_scale",
        "fairness_window_minutes",
        "queue_timeout_secs",
    ];

    for (key, value) in &req {
        if !valid_keys.contains(&key.as_str()) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("Unknown setting: {key}") })),
            )
                .into_response();
        }

        let value_str = match value {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s.clone(),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("Invalid value for {key}: expected number or string") })),
                )
                    .into_response();
            }
        };

        if let Err(e) = save_setting(&state.db, key, &value_str).await {
            return error::internal_error("update_settings:save", e);
        }
    }

    // Refresh the cached settings
    if let Err(e) = state.scheduler.reload_settings(&state.db).await {
        error!(error = %e, "Failed to reload settings after update");
    }

    info!(target: "audit", action = "settings.update", actor = %session.user_id, keys = ?req.keys().collect::<Vec<_>>(), "Admin updated settings");

    // Return the updated settings
    let settings = state.scheduler.settings().await;
    Json(serde_json::json!({
        "fairness_base_priority": settings.base_priority,
        "fairness_wait_weight": settings.wait_weight,
        "fairness_usage_weight": settings.usage_weight,
        "fairness_usage_scale": settings.usage_scale,
        "fairness_window_minutes": settings.window_minutes,
        "queue_timeout_secs": settings.queue_timeout_secs,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Usage Analytics (admin-wide)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AdminUsageQuery {
    period: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct AdminUsageByUser {
    user_label: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
}

/// GET /api/admin/usage — Global usage statistics with per-user breakdown.
async fn admin_usage(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AdminUsageQuery>,
) -> impl IntoResponse {
    let period = params.period.unwrap_or_else(|| "day".to_string());
    let interval = common::period_to_interval(&period);

    // Global summary
    let summary: (i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(COUNT(*), 0), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) FROM usage_log WHERE created_at >= datetime('now', ?)",
    )
    .bind(interval)
    .fetch_one(&state.db.pool)
    .await
    .unwrap_or((0, 0, 0));

    // Per-user breakdown
    let by_user = sqlx::query_as::<_, AdminUsageByUser>(
        r#"
        SELECT COALESCE(u.display_name, u.email, ul.user_id) as user_label,
               COUNT(*) as requests,
               COALESCE(SUM(ul.input_tokens), 0) as input_tokens,
               COALESCE(SUM(ul.output_tokens), 0) as output_tokens
        FROM usage_log ul
        LEFT JOIN users u ON u.id = ul.user_id
        WHERE ul.created_at >= datetime('now', ?)
        GROUP BY ul.user_id
        ORDER BY requests DESC
        "#,
    )
    .bind(interval)
    .fetch_all(&state.db.pool)
    .await
    .unwrap_or_default();

    Json(serde_json::json!({
        "summary": {
            "total_requests": summary.0,
            "total_input_tokens": summary.1,
            "total_output_tokens": summary.2,
            "period": period,
        },
        "by_user": by_user,
    }))
    .into_response()
}

/// GET /api/admin/usage/timeline — Time-series usage grouped by user.
async fn admin_usage_timeline(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AdminUsageQuery>,
) -> impl IntoResponse {
    let period = params.period.unwrap_or_else(|| "day".to_string());
    let (interval, time_bucket) = common::period_to_interval_and_bucket(&period);

    let timeline = sqlx::query_as::<_, (String, String, i64, i64, i64)>(&format!(
        r#"
            SELECT strftime('{}', ul.created_at) as ts,
                   COALESCE(u.display_name, u.email, ul.user_id) as user_label,
                   COUNT(*) as requests,
                   COALESCE(SUM(ul.input_tokens), 0) as input_tokens,
                   COALESCE(SUM(ul.output_tokens), 0) as output_tokens
            FROM usage_log ul
            LEFT JOIN users u ON u.id = ul.user_id
            WHERE ul.created_at >= datetime('now', ?)
            GROUP BY ts, ul.user_id
            ORDER BY ts
            "#,
        time_bucket
    ))
    .bind(interval)
    .fetch_all(&state.db.pool)
    .await
    .unwrap_or_default();

    let timeline_json: Vec<serde_json::Value> = timeline
        .into_iter()
        .map(
            |(timestamp, user_label, requests, input_tokens, output_tokens)| {
                serde_json::json!({
                    "timestamp": timestamp,
                    "user_label": user_label,
                    "requests": requests,
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                })
            },
        )
        .collect();

    Json(serde_json::json!({
        "timeline": timeline_json,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// KV cache estimation (extracted for testability)
// ---------------------------------------------------------------------------

/// Parameters for KV cache estimation.
struct KvCacheParams {
    n_layers: Option<i64>,
    n_heads: Option<i64>,
    n_kv_heads: Option<i64>,
    embedding_length: Option<i64>,
    key_length: Option<i64>,
    value_length: Option<i64>,
    /// SWA-aware pre-aggregate: Σ over full-context layers of
    /// `kv_heads_i × (key_len + val_len) × 2` bytes/token. When present, the
    /// estimator uses the SWA-aware formula; otherwise it falls back to the
    /// legacy derived-head-dim calculation.
    kv_bytes_per_token_global: Option<i64>,
    /// SWA-aware pre-aggregate: Σ over sliding-window layers of
    /// `kv_heads_i × (key_len_swa + val_len_swa) × 2` bytes/token. Only the
    /// first `sliding_window` tokens of context consume this per token.
    kv_bytes_per_token_swa: Option<i64>,
    /// Sliding-window size (from GGUF `<arch>.attention.sliding_window`). When
    /// None but `kv_bytes_per_token_swa` is Some, the SWA layers still cap at
    /// full context — conservative upper bound.
    sliding_window: Option<i64>,
    context_size: u64,
    parallel: u64,
}

/// Estimate KV cache size in MB.
///
/// Two paths:
///
/// 1. **SWA-aware** (preferred): when
///    `kv_bytes_per_token_global` is set — computed at ingestion from the
///    per-layer `head_count_kv` array and `sliding_window_pattern`. The
///    formula is
///
///    ```text
///    kv_bytes = (global_bpt × context
///              + swa_bpt     × min(context, sliding_window)) × parallel
///    ```
///
///    This correctly accounts for models like Gemma 3/4 that interleave
///    global and sliding-window layers with different head counts and dims.
///
/// 2. **Legacy derived** (fallback for models lacking the aggregates):
///    uses explicit `key_length` / `value_length` when available (for
///    models like Gemma where head_dim != embedding_length / n_heads),
///    else derives `head_dim = embedding_length / n_heads`.
fn estimate_kv_cache_mb(p: &KvCacheParams) -> u64 {
    if let Some(global_bpt) = p.kv_bytes_per_token_global {
        let global_bytes = (global_bpt as u64).saturating_mul(p.context_size);
        let swa_bytes = match p.kv_bytes_per_token_swa {
            Some(swa_bpt) => {
                let swa_tokens = match p.sliding_window {
                    Some(w) if (w as u64) < p.context_size => w as u64,
                    _ => p.context_size,
                };
                (swa_bpt as u64).saturating_mul(swa_tokens)
            }
            None => 0,
        };
        let kv_bytes = global_bytes
            .saturating_add(swa_bytes)
            .saturating_mul(p.parallel);
        return kv_bytes / (1024 * 1024);
    }

    match (p.n_layers, p.n_heads, p.n_kv_heads, p.embedding_length) {
        (Some(layers), Some(heads), Some(kv_heads), Some(emb_len)) if heads > 0 => {
            let derived_head_dim = emb_len as u64 / heads as u64;
            let key_dim = p.key_length.map(|k| k as u64).unwrap_or(derived_head_dim);
            let val_dim = p.value_length.map(|v| v as u64).unwrap_or(derived_head_dim);
            let kv_bytes = (layers as u64)
                * (kv_heads as u64)
                * (key_dim + val_dim)
                * p.context_size
                * p.parallel
                * 2; // fp16
            kv_bytes / (1024 * 1024)
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Phase 7: backend crash diagnostics
// ---------------------------------------------------------------------------

/// One row of the crash history list, returned by
/// `GET /api/admin/models/:id/crash_history`.
#[derive(Debug, Serialize, sqlx::FromRow)]
struct CrashHistoryRow {
    occurred_at: String,
    container_id: Option<String>,
    exit_code: Option<i64>,
    oom_killed: i64,
    signal: Option<String>,
    /// Indicates that the on-disk log tail file may still be available —
    /// callers should still defensively handle a 404 from
    /// `GET /api/admin/models/:id/crash_log/:occurred_at` because the
    /// inline GC may have evicted the file in the meantime.
    log_path_present: bool,
}

/// `GET /api/admin/models/:id/crash_history` — last 5 crash events.
///
/// Used by the admin UI to render an expandable per-model crash panel
/// (timestamp, exit code, OOMKilled flag, link to the captured log tail).
async fn get_crash_history(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    #[derive(sqlx::FromRow)]
    struct Row {
        occurred_at: String,
        container_id: Option<String>,
        exit_code: Option<i64>,
        oom_killed: i64,
        signal: Option<String>,
        log_path: Option<String>,
    }

    match sqlx::query_as::<_, Row>(
        "SELECT occurred_at, container_id, exit_code, oom_killed, signal, log_path \
         FROM backend_crash_log WHERE model_id = ? \
         ORDER BY occurred_at DESC LIMIT 5",
    )
    .bind(&model_id)
    .fetch_all(&state.db.pool)
    .await
    {
        Ok(rows) => {
            let history: Vec<CrashHistoryRow> = rows
                .into_iter()
                .map(|r| CrashHistoryRow {
                    occurred_at: r.occurred_at,
                    container_id: r.container_id,
                    exit_code: r.exit_code,
                    oom_killed: r.oom_killed,
                    signal: r.signal,
                    log_path_present: r.log_path.is_some(),
                })
                .collect();
            Json(serde_json::json!({ "history": history })).into_response()
        }
        Err(e) => error::internal_error("crash_history", e),
    }
}

/// `GET /api/admin/models/:id/crash_log/:occurred_at` — stream a captured log tail.
///
/// Returns the bytes of the on-disk log tail file as `text/plain; charset=utf-8`.
/// 404 if no row matches, the row had no `log_path`, or the file has been GC'd.
async fn get_crash_log(
    State(state): State<Arc<AppState>>,
    Path((model_id, occurred_at)): Path<(String, String)>,
) -> axum::response::Response {
    // 1. Look up the row.
    let row: Option<(Option<String>,)> = match sqlx::query_as(
        "SELECT log_path FROM backend_crash_log WHERE model_id = ? AND occurred_at = ?",
    )
    .bind(&model_id)
    .bind(&occurred_at)
    .fetch_optional(&state.db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return error::internal_error("crash_log:lookup", e),
    };

    let log_path = match row {
        Some((Some(p),)) => p,
        Some((None,)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "log was not captured" })),
            )
                .into_response();
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "crash log not found" })),
            )
                .into_response();
        }
    };

    // 2. Read the file. Crash logs are capped (last ~500 lines) so a
    //    full read into memory is fine — far simpler than streaming.
    match tokio::fs::read(&log_path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            bytes,
        )
            .into_response(),
        Err(e) => {
            // File missing (likely GC'd) is the common case here — log at
            // debug, not error, so production logs aren't noisy.
            tracing::debug!(
                model = %model_id,
                occurred_at = %occurred_at,
                path = %log_path,
                error = %e,
                "crash_log: file no longer available",
            );
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "log file no longer available" })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- estimate_kv_cache_mb -------------------------------------------------

    /// Helper to build KvCacheParams with common defaults (legacy path:
    /// no SWA aggregates).
    #[allow(clippy::too_many_arguments)]
    fn kv_params(
        n_layers: Option<i64>,
        n_heads: Option<i64>,
        n_kv_heads: Option<i64>,
        embedding_length: Option<i64>,
        key_length: Option<i64>,
        value_length: Option<i64>,
        context_size: u64,
        parallel: u64,
    ) -> KvCacheParams {
        KvCacheParams {
            n_layers,
            n_heads,
            n_kv_heads,
            embedding_length,
            key_length,
            value_length,
            kv_bytes_per_token_global: None,
            kv_bytes_per_token_swa: None,
            sliding_window: None,
            context_size,
            parallel,
        }
    }

    /// Helper to build KvCacheParams for the SWA-aware path. The legacy
    /// fields are all `None` so the estimator exercises the aggregates path.
    fn kv_params_swa(
        kv_bytes_per_token_global: Option<i64>,
        kv_bytes_per_token_swa: Option<i64>,
        sliding_window: Option<i64>,
        context_size: u64,
        parallel: u64,
    ) -> KvCacheParams {
        KvCacheParams {
            n_layers: None,
            n_heads: None,
            n_kv_heads: None,
            embedding_length: None,
            key_length: None,
            value_length: None,
            kv_bytes_per_token_global,
            kv_bytes_per_token_swa,
            sliding_window,
            context_size,
            parallel,
        }
    }

    #[test]
    fn kv_cache_returns_zero_when_metadata_missing() {
        // n_kv_heads = None → can't estimate
        let p = kv_params(Some(32), Some(32), None, Some(4096), None, None, 2048, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 0);
        // n_layers = None
        let p = kv_params(None, Some(32), Some(8), Some(4096), None, None, 2048, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 0);
        // all None
        let p = kv_params(None, None, None, None, None, None, 2048, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 0);
    }

    #[test]
    fn kv_cache_derived_head_dim_standard_model() {
        // Llama-style: 32 layers, 32 heads, 8 kv_heads, emb=4096
        // head_dim = 4096/32 = 128, key_dim=val_dim=128
        // kv_bytes = 32 * 8 * (128+128) * 2048 * 1 * 2 = 268_435_456
        // kv_mb = 268_435_456 / 1_048_576 = 256
        let p = kv_params(Some(32), Some(32), Some(8), Some(4096), None, None, 2048, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 256);
    }

    #[test]
    fn kv_cache_explicit_key_value_lengths_gemma4() {
        // Gemma 4 style: 34 layers, 32 heads, 8 kv_heads, emb=3584
        // Explicit key_length=512, value_length=512
        // derived head_dim would be 3584/32=112 (WRONG for Gemma 4)
        // With explicit dims: kv_bytes = 34 * 8 * (512+512) * 8192 * 1 * 2
        let p = kv_params(
            Some(34),
            Some(32),
            Some(8),
            Some(3584),
            Some(512),
            Some(512),
            8192,
            1,
        );
        // 34 * 8 * 1024 * 8192 * 2 = 4_563_402_752 bytes = 4352 MB
        assert_eq!(estimate_kv_cache_mb(&p), 4352);

        // Verify this differs from the (wrong) derived path
        let p_derived = kv_params(Some(34), Some(32), Some(8), Some(3584), None, None, 8192, 1);
        // 34 * 8 * (112+112) * 8192 * 2 = 998_244_352 bytes = 952 MB (undercounts!)
        let mb_derived = estimate_kv_cache_mb(&p_derived);
        assert_eq!(mb_derived, 952);
        assert!(
            estimate_kv_cache_mb(&p) > mb_derived,
            "Explicit dims should give larger (correct) estimate"
        );
    }

    #[test]
    fn kv_cache_parallel_slots_multiply() {
        let p1 = kv_params(Some(32), Some(32), Some(8), Some(4096), None, None, 2048, 1);
        let p4 = kv_params(Some(32), Some(32), Some(8), Some(4096), None, None, 2048, 4);
        assert_eq!(estimate_kv_cache_mb(&p4), estimate_kv_cache_mb(&p1) * 4);
    }

    #[test]
    fn kv_cache_zero_heads_returns_zero() {
        // heads = 0 should not panic (division by zero guard)
        let p = kv_params(Some(32), Some(0), Some(8), Some(4096), None, None, 2048, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 0);
    }

    // -- SWA-aware estimator --------------------------------------------------

    #[test]
    fn kv_cache_swa_aware_gemma4() {
        // Gemma 4 31B at 256K context. Per the problem statement:
        //   10 global layers × 4 kv_heads × (512+512) × 2 = 81_920 bytes/token
        //   50 SWA    layers × 16 kv_heads × (256+256) × 2 = 819_200 bytes/token
        //   sliding_window = 1024, context = 262_144
        //
        //   kv_bytes = 81_920 * 262_144 + 819_200 * 1024
        //            = 21_474_836_480 + 838_860_800
        //            = 22_313_697_280
        //            = 21_280 MB
        let global_bpt: i64 = 10 * 4 * (512 + 512) * 2;
        assert_eq!(global_bpt, 81_920);
        let swa_bpt: i64 = 50 * 16 * (256 + 256) * 2;
        assert_eq!(swa_bpt, 819_200);

        let p = kv_params_swa(Some(global_bpt), Some(swa_bpt), Some(1024), 262_144, 1);
        let mb = estimate_kv_cache_mb(&p);
        // Allow ±1 MB for rounding noise (the spec calls for ≈21,280 MB).
        assert!(mb.abs_diff(21_280) <= 1, "expected ~21280 MB, got {mb}");
    }

    #[test]
    fn kv_cache_legacy_fallback_when_aggregates_null() {
        // Model without SWA aggregates must fall through to the legacy path
        // and match exactly what the pre-SWA estimator produced.
        let p = kv_params(Some(32), Some(32), Some(8), Some(4096), None, None, 2048, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 256);
    }

    #[test]
    fn kv_cache_no_sliding_window_swa_uses_full_context() {
        // If sliding_window is None but swa_bpt is Some, treat SWA layers
        // as spanning the full context (conservative upper bound).
        // 2 layers × 8 heads × (128+128) × 2 = 8_192 bytes/token per half,
        // so with global_bpt = swa_bpt = 8_192 and context = 1024, parallel = 1:
        //   kv_bytes = 8_192 * 1024 + 8_192 * 1024 = 16_777_216 bytes = 16 MB
        let p = kv_params_swa(Some(8_192), Some(8_192), None, 1024, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 16);

        // And it should match what you'd get if sliding_window >= context:
        let p_large = kv_params_swa(Some(8_192), Some(8_192), Some(10_000), 1024, 1);
        assert_eq!(estimate_kv_cache_mb(&p_large), 16);
    }

    #[test]
    fn kv_cache_swa_aware_parallel_multiplies() {
        // parallel slots multiply the final kv_bytes, same as legacy path.
        let p1 = kv_params_swa(Some(81_920), Some(819_200), Some(1024), 262_144, 1);
        let p4 = kv_params_swa(Some(81_920), Some(819_200), Some(1024), 262_144, 4);
        assert_eq!(estimate_kv_cache_mb(&p4), estimate_kv_cache_mb(&p1) * 4);
    }

    #[test]
    fn kv_cache_swa_aware_only_global_layers() {
        // If swa_bpt is None (no SWA layers / homogeneous model wrapped into
        // the aggregates path), the SWA term is zero and the answer equals
        // global_bpt × context / (1024*1024).
        // 4 layers × 8 heads × (128+128) × 2 = 16_384 bytes/token,
        // context 2048 → 33_554_432 bytes → 32 MB.
        let p = kv_params_swa(Some(16_384), None, Some(1024), 2048, 1);
        assert_eq!(estimate_kv_cache_mb(&p), 32);
    }
}
