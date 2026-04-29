use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::auth::tokens;
use crate::auth::AuthUser;
use crate::proxy::streaming::proxy_to_backend;
use crate::scheduler::usage;
use crate::AppState;

pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/chat/completions", post(chat_completions))
        .route("/completions", post(completions))
        .route("/models", get(list_models))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct ChatCompletionRequest {
    model: String,
    #[serde(default)]
    stream: bool,
    /// OpenAI `user` field — Open WebUI populates this with the user's email.
    user: Option<String>,
    // All other fields are passed through to the backend
}

/// Extract token usage from a non-streaming OpenAI response body.
fn extract_usage_from_response(body: &[u8]) -> (i64, i64) {
    #[derive(Deserialize)]
    struct UsageInfo {
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
    }
    #[derive(Deserialize)]
    struct ResponseWithUsage {
        usage: Option<UsageInfo>,
    }

    match serde_json::from_slice::<ResponseWithUsage>(body) {
        Ok(resp) => {
            if let Some(u) = resp.usage {
                (
                    u.prompt_tokens.unwrap_or(0),
                    u.completion_tokens.unwrap_or(0),
                )
            } else {
                (0, 0)
            }
        }
        Err(_) => (0, 0),
    }
}

/// Common logic for both chat and text completions: resolve model, proxy, log usage.
async fn proxy_completion(
    state: Arc<AppState>,
    auth_user: AuthUser,
    body: Bytes,
    parsed_model: &str,
    is_streaming: bool,
    backend_path: &str,
    user_email_override: Option<&str>,
) -> Response<Body> {
    let start = Instant::now();

    // Resolve model using the scheduler, with token's constraints
    let model = match state
        .scheduler
        .resolve_model(
            &state.db,
            parsed_model,
            auth_user.category_id.as_deref(),
            auth_user.specific_model_id.as_deref(),
        )
        .await
    {
        Ok(m) => m,
        Err(e) => {
            error!(error = %e, model = %parsed_model, "Model resolution failed");
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": {
                        "message": format!("Model not found: {}", parsed_model),
                        "type": "invalid_request_error",
                        "code": "model_not_found"
                    }
                })),
            )
                .into_response();
        }
    };

    if !model.loaded {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": {
                    "message": if model.hf_repo == parsed_model {
                        format!("Model '{}' is not currently loaded", parsed_model)
                    } else {
                        format!("Model '{}' (overridden by token from '{}') is not currently loaded", model.hf_repo, parsed_model)
                    },
                    "type": "server_error",
                    "code": "model_not_loaded"
                }
            })),
        )
            .into_response();
    }

    // If system is reserved, only the reservation holder may proceed.
    // Internal tokens (Open WebUI) are exempt — gated at the webui proxy level.
    if !auth_user.is_internal {
        if let Some(active) = state.scheduler.active_reservation().await {
            if active.user_id != auth_user.user_id {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({
                        "error": {
                            "message": "System is currently reserved for exclusive use",
                            "type": "server_error",
                            "code": "system_reserved"
                        }
                    })),
                )
                    .into_response();
            }
        }
    }

    // Acquire a concurrency slot (holds connection, times out with 429)
    let queue_start = Instant::now();
    let settings = state.scheduler.settings().await;
    let timeout = Duration::from_secs(settings.queue_timeout_secs);
    let _slot = match state
        .scheduler
        .gate()
        .acquire_with_timeout(
            &model.id,
            &auth_user.user_id,
            &state.db,
            &settings,
            state.scheduler.queue(),
            timeout,
        )
        .await
    {
        Ok(slot) => slot,
        Err(_) => {
            warn!(
                model = %model.id,
                user = %auth_user.user_id,
                "Request timed out in queue"
            );
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", settings.queue_timeout_secs.to_string())],
                Json(serde_json::json!({
                    "error": {
                        "message": "Server is busy. Please retry later.",
                        "type": "server_error",
                        "code": "queue_timeout"
                    }
                })),
            )
                .into_response();
        }
    };
    let queued_ms = queue_start.elapsed().as_millis() as i64;

    // Reach backend via container name on the internal Docker network
    let backend_url = format!(
        "{}{}",
        state
            .docker
            .backend_base_url(&model.id, &model.backend_type),
        backend_path
    );
    let client = reqwest::Client::new();

    // Look up the per-container API key for backend authentication
    let api_key: Option<String> =
        sqlx::query_as::<_, (String,)>("SELECT api_key FROM container_secrets WHERE model_id = ?")
            .bind(&model.id)
            .fetch_optional(&state.db.pool)
            .await
            .ok()
            .flatten()
            .map(|(key,)| key);

    let result = proxy_to_backend(
        &client,
        &backend_url,
        body,
        is_streaming,
        api_key.as_deref(),
        &model.id,
        &state.probe_tx,
    )
    .await;

    let latency_ms = start.elapsed().as_millis() as i64;

    // Extract token usage from non-streaming responses
    let (input_tokens, output_tokens) = result
        .body_bytes
        .as_ref()
        .map(|b| extract_usage_from_response(b))
        .unwrap_or((0, 0));

    // Meta token resolution: if this is an internal token (Open WebUI) and the
    // request includes a `user` email, attribute usage to the actual user.
    let (log_user_id, log_token_id) = if auth_user.is_internal {
        if let Some(email) = user_email_override {
            match tokens::resolve_meta_user(&state.db, email).await {
                Ok(Some(meta)) => (meta.user_id, meta.token_id),
                Ok(None) => {
                    warn!(email = %email, "Meta resolution: no user found for email");
                    (auth_user.user_id.clone(), auth_user.token_id.clone())
                }
                Err(e) => {
                    warn!(error = %e, email = %email, "Meta resolution: lookup failed");
                    (auth_user.user_id.clone(), auth_user.token_id.clone())
                }
            }
        } else {
            (auth_user.user_id.clone(), auth_user.token_id.clone())
        }
    } else {
        (auth_user.user_id.clone(), auth_user.token_id.clone())
    };

    let db = state.db.clone();
    let token_id = log_token_id;
    let user_id = log_user_id;
    let model_id = model.id.clone();
    let category_id = model.category_id.clone();

    tokio::spawn(async move {
        let entry = usage::UsageEntry {
            token_id: &token_id,
            user_id: &user_id,
            model_id: &model_id,
            category_id: category_id.as_deref(),
            input_tokens,
            output_tokens,
            latency_ms,
            queued_ms,
        };
        if let Err(e) = usage::log_usage(&db, &entry).await {
            warn!(error = %e, "Failed to log usage");
        }
    });

    result.response
}

/// POST /v1/chat/completions -- OpenAI-compatible chat completion endpoint.
/// Resolves the model, proxies to the appropriate llama.cpp backend, logs usage.
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    Extension(auth_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let parsed: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return Json(serde_json::json!({
                "error": {
                    "message": format!("Invalid request body: {}", e),
                    "type": "invalid_request_error",
                    "code": "invalid_body"
                }
            }))
            .into_response();
        }
    };

    info!(
        model = %parsed.model,
        stream = parsed.stream,
        user_id = %auth_user.user_id,
        "Chat completion request"
    );

    // Prefer X-OpenWebUI-User-Email header, fall back to body `user` field
    let header_email = headers
        .get("X-OpenWebUI-User-Email")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let user_email = header_email.as_deref().or(parsed.user.as_deref());

    proxy_completion(
        state,
        auth_user,
        body,
        &parsed.model,
        parsed.stream,
        "/v1/chat/completions",
        user_email,
    )
    .await
}

/// POST /v1/completions -- OpenAI-compatible text completion endpoint.
async fn completions(
    State(state): State<Arc<AppState>>,
    Extension(auth_user): Extension<AuthUser>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let parsed: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return Json(serde_json::json!({
                "error": {
                    "message": format!("Invalid request body: {}", e),
                    "type": "invalid_request_error",
                    "code": "invalid_body"
                }
            }))
            .into_response();
        }
    };

    info!(
        model = %parsed.model,
        stream = parsed.stream,
        user_id = %auth_user.user_id,
        "Text completion request"
    );

    let header_email = headers
        .get("X-OpenWebUI-User-Email")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let user_email = header_email.as_deref().or(parsed.user.as_deref());

    proxy_completion(
        state,
        auth_user,
        body,
        &parsed.model,
        parsed.stream,
        "/v1/completions",
        user_email,
    )
    .await
}

#[derive(Debug, Serialize)]
struct ModelInfo {
    id: String,
    object: &'static str,
    owned_by: &'static str,
}

#[derive(Debug, Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelInfo>,
}

/// GET /v1/models -- List available models (OpenAI-compatible).
async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let models: Vec<(String, String)> =
        match sqlx::query_as("SELECT id, hf_repo FROM models WHERE loaded = 1")
            .fetch_all(&state.db.pool)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                error!(error = %e, "Failed to query models");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": {
                            "message": "Failed to list models",
                            "type": "server_error"
                        }
                    })),
                )
                    .into_response();
            }
        };

    let data: Vec<ModelInfo> = models
        .into_iter()
        .map(|(_, hf_repo)| ModelInfo {
            id: hf_repo,
            object: "model",
            owned_by: "sovereign-engine",
        })
        .collect();

    Json(ModelsResponse {
        object: "list",
        data,
    })
    .into_response()
}
