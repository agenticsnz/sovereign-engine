//! Shared helpers extracted from admin/user/reservation handlers to reduce
//! code duplication. Only genuinely repeated patterns live here — we do NOT
//! over-abstract.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use sqlx::SqlitePool;
use tracing::error;
use uuid::Uuid;

use super::error;
use crate::db::models::{Model, ModelCategory};
use crate::docker::runtime_overrides::ModelRuntimeOverrides;
use crate::metrics::ContainerStatus;
use crate::AppState;

// ---------------------------------------------------------------------------
// Period → SQL interval mapping (used by usage & timeline endpoints)
// ---------------------------------------------------------------------------

/// Converts a user-facing period string ("hour", "day", "week", "month") into
/// the SQLite datetime offset used in `WHERE created_at >= datetime('now', ?)`.
pub fn period_to_interval(period: &str) -> &'static str {
    match period {
        "hour" => "-1 hour",
        "day" => "-1 day",
        "week" => "-7 days",
        "month" => "-30 days",
        _ => "-1 day",
    }
}

/// Converts a user-facing period string into both a SQLite interval and a
/// `strftime` time-bucket format string (for timeline grouping).
pub fn period_to_interval_and_bucket(period: &str) -> (&'static str, &'static str) {
    match period {
        "hour" => ("-1 hour", "%Y-%m-%dT%H:%M:00"),
        "day" => ("-1 day", "%Y-%m-%dT%H:00:00"),
        "week" => ("-7 days", "%Y-%m-%d"),
        "month" => ("-30 days", "%Y-%m-%d"),
        _ => ("-1 day", "%Y-%m-%dT%H:00:00"),
    }
}

// ---------------------------------------------------------------------------
// Shared read-only list queries (categories, models)
// ---------------------------------------------------------------------------

/// Fetch all model categories. Used by both admin and user list endpoints.
pub async fn fetch_all_categories(pool: &SqlitePool) -> impl IntoResponse {
    match sqlx::query_as::<_, ModelCategory>(
        "SELECT id, name, description, preferred_model_id, created_at FROM model_categories",
    )
    .fetch_all(pool)
    .await
    {
        Ok(categories) => Json(serde_json::json!({ "categories": categories })).into_response(),
        Err(e) => error::internal_error("list_categories", e),
    }
}

/// Fetch all registered models. Used by both admin and user list endpoints.
pub async fn fetch_all_models(pool: &SqlitePool) -> impl IntoResponse {
    match sqlx::query_as::<_, Model>(
        "SELECT id, hf_repo, filename, size_bytes, category_id, loaded, backend_port, backend_type, last_used_at, created_at, context_length, n_layers, n_heads, n_kv_heads, embedding_length, key_length, value_length, sliding_window, kv_bytes_per_token_global, kv_bytes_per_token_swa, mmproj_filename, runtime_overrides FROM models",
    )
    .fetch_all(pool)
    .await
    {
        Ok(models) => Json(serde_json::json!({ "models": models })).into_response(),
        Err(e) => error::internal_error("list_models", e),
    }
}

// ---------------------------------------------------------------------------
// Container label extraction (shared between admin system_status & metrics)
// ---------------------------------------------------------------------------

/// Extract container status info from a list of Docker container summaries.
/// Merges per-container VRAM data from the provided map.
pub fn extract_container_statuses(
    containers: Vec<bollard::models::ContainerSummary>,
    vram_map: &std::collections::HashMap<String, u64>,
) -> Vec<ContainerStatus> {
    containers
        .into_iter()
        .map(|c| {
            let labels = c.labels.as_ref();
            let model_id = labels
                .and_then(|l| l.get("sovereign-engine.model-id"))
                .cloned()
                .unwrap_or_default();
            let backend_type = labels
                .and_then(|l| l.get("sovereign-engine.backend"))
                .cloned()
                .unwrap_or_else(|| "llamacpp".to_string());
            let healthy = c.state == Some(bollard::models::ContainerSummaryStateEnum::RUNNING);
            let vram_used_mb = vram_map.get(&model_id).copied();
            ContainerStatus {
                model_id,
                backend_type,
                healthy,
                state: c.state.map(|s| format!("{:?}", s).to_lowercase()),
                vram_used_mb,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Container lifecycle: start + post-start bookkeeping
// ---------------------------------------------------------------------------

/// Request fields common to both admin and reservation container start.
pub struct StartContainerParams {
    pub model_id: String,
    pub backend_type: Option<String>,
    pub gpu_type: Option<String>,
    pub gpu_layers: Option<u32>,
    pub parallel: Option<u32>,
}

/// Row from `models` needed by the start-container flow.
#[derive(sqlx::FromRow)]
pub struct ModelStartRow {
    pub id: String,
    pub hf_repo: String,
    pub filename: Option<String>,
    pub backend_type: String,
    pub context_length: Option<i64>,
    /// JSON blob; deserialized into [`ModelRuntimeOverrides`] in the start path.
    /// Stored as text per the `runtime_overrides` column.
    pub runtime_overrides: String,
}

/// Core container-start logic shared between admin and reservation handlers.
///
/// On success, returns `Ok((container_name, backend_type_used))`.
/// On failure, returns an `Err(axum::response::Response)` ready to send.
pub async fn start_container_core(
    state: &Arc<AppState>,
    params: &StartContainerParams,
) -> Result<(String, String), axum::response::Response> {
    // Look up the model
    let model: Option<ModelStartRow> = sqlx::query_as(
        "SELECT id, hf_repo, filename, backend_type, context_length, runtime_overrides FROM models WHERE id = ?",
    )
    .bind(&params.model_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| error::internal_error("start_container:lookup", e))?;

    let ModelStartRow {
        id: model_id,
        hf_repo,
        filename,
        backend_type: db_backend_type,
        context_length: db_context_length,
        runtime_overrides: runtime_overrides_json,
    } = model.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Model not found" })),
        )
            .into_response()
    })?;

    let context_size = match db_context_length {
        Some(v) => v as u32,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Model has no context_length set — cannot start container. Re-download or manually set context_length in the database." })),
            )
                .into_response());
        }
    };

    let backend_type = params.backend_type.as_deref().unwrap_or(&db_backend_type);

    // Allocate a collision-free UID and generate a per-container API key
    let uid = state
        .docker
        .allocate_uid()
        .await
        .map_err(|e| error::internal_error("start_container:allocate_uid", e))?;
    let api_key = Uuid::new_v4().to_string();

    let container_result = match backend_type {
        "llamacpp" => {
            let safe_repo = hf_repo.replace('/', "--");
            let gguf_path = match &filename {
                Some(f) => format!("{}/{}", safe_repo, f),
                None => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": "No filename recorded for this model — cannot determine GGUF file path" })),
                    )
                        .into_response());
                }
            };

            let parallel = params.parallel.unwrap_or(1).max(1);
            // A bad JSON blob in the DB shouldn't keep the model from starting —
            // fall back to defaults (i.e. no overrides) and carry on.
            let overrides = serde_json::from_str::<ModelRuntimeOverrides>(&runtime_overrides_json)
                .unwrap_or_default();
            let llamacpp_config = crate::docker::llamacpp::LlamacppConfig {
                model_id: model_id.clone(),
                gguf_path,
                mmproj_path: None,
                gpu_type: crate::docker::llamacpp::GpuType::from_str(
                    params.gpu_type.as_deref().unwrap_or("none"),
                ),
                gpu_layers: params.gpu_layers.unwrap_or(99),
                context_size,
                parallel,
                extra_args: overrides.to_cli_args(),
                uid,
                api_key: api_key.clone(),
            };
            state.docker.start_llamacpp(&llamacpp_config).await
        }
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("Unknown backend type: {other}") })),
            )
                .into_response());
        }
    };

    match container_result {
        Ok(container_name) => {
            // Post-start bookkeeping: persist secrets, register gate, mark loaded
            let parallel_slots = params.parallel.unwrap_or(1).max(1);
            if let Err(e) = sqlx::query(
                "INSERT OR REPLACE INTO container_secrets (model_id, container_uid, api_key, parallel_slots) VALUES (?, ?, ?, ?)",
            )
            .bind(&model_id)
            .bind(uid as i64)
            .bind(&api_key)
            .bind(parallel_slots as i64)
            .execute(&state.db.pool)
            .await
            {
                error!(model = %model_id, error = %e, "Failed to persist container secrets");
            }

            state
                .scheduler
                .gate()
                .register(&model_id, parallel_slots)
                .await;

            let _ = sqlx::query("UPDATE models SET loaded = 1 WHERE id = ?")
                .bind(&model_id)
                .execute(&state.db.pool)
                .await;

            let url = state
                .docker
                .backend_base_url(&model_id, backend_type)
                .to_string();

            Ok((container_name, url))
        }
        Err(e) => {
            error!(model = %model_id, backend = %backend_type, error = ?e, "Failed to start container");
            Err(error::internal_error("start_container", e))
        }
    }
}

// ---------------------------------------------------------------------------
// Container lifecycle: post-stop cleanup
// ---------------------------------------------------------------------------

/// Shared cleanup after stopping a container: unregister gate, delete secrets,
/// mark model as unloaded.
pub async fn post_stop_cleanup(state: &Arc<AppState>, model_id: &str) {
    state.scheduler.gate().unregister(model_id).await;
    let _ = sqlx::query("DELETE FROM container_secrets WHERE model_id = ?")
        .bind(model_id)
        .execute(&state.db.pool)
        .await;
    let _ = sqlx::query("UPDATE models SET loaded = 0 WHERE id = ?")
        .bind(model_id)
        .execute(&state.db.pool)
        .await;
}

/// Look up backend_type for a model, defaulting to "llamacpp" on any failure.
pub async fn lookup_backend_type(pool: &SqlitePool, model_id: &str) -> String {
    match sqlx::query_as::<_, (String,)>("SELECT backend_type FROM models WHERE id = ?")
        .bind(model_id)
        .fetch_optional(pool)
        .await
    {
        Ok(Some((bt,))) => bt,
        _ => "llamacpp".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{ContainerSummary, ContainerSummaryStateEnum};
    use std::collections::HashMap;

    // -----------------------------------------------------------------------
    // period_to_interval
    // -----------------------------------------------------------------------

    #[test]
    fn period_to_interval_hour() {
        assert_eq!(period_to_interval("hour"), "-1 hour");
    }

    #[test]
    fn period_to_interval_day() {
        assert_eq!(period_to_interval("day"), "-1 day");
    }

    #[test]
    fn period_to_interval_week() {
        assert_eq!(period_to_interval("week"), "-7 days");
    }

    #[test]
    fn period_to_interval_month() {
        assert_eq!(period_to_interval("month"), "-30 days");
    }

    #[test]
    fn period_to_interval_unknown_defaults_to_day() {
        assert_eq!(period_to_interval("year"), "-1 day");
        assert_eq!(period_to_interval("HOUR"), "-1 day"); // case-sensitive
    }

    #[test]
    fn period_to_interval_empty_defaults_to_day() {
        assert_eq!(period_to_interval(""), "-1 day");
    }

    // -----------------------------------------------------------------------
    // period_to_interval_and_bucket
    // -----------------------------------------------------------------------

    #[test]
    fn period_to_interval_and_bucket_hour() {
        assert_eq!(
            period_to_interval_and_bucket("hour"),
            ("-1 hour", "%Y-%m-%dT%H:%M:00")
        );
    }

    #[test]
    fn period_to_interval_and_bucket_day() {
        assert_eq!(
            period_to_interval_and_bucket("day"),
            ("-1 day", "%Y-%m-%dT%H:00:00")
        );
    }

    #[test]
    fn period_to_interval_and_bucket_week() {
        assert_eq!(
            period_to_interval_and_bucket("week"),
            ("-7 days", "%Y-%m-%d")
        );
    }

    #[test]
    fn period_to_interval_and_bucket_month() {
        assert_eq!(
            period_to_interval_and_bucket("month"),
            ("-30 days", "%Y-%m-%d")
        );
    }

    #[test]
    fn period_to_interval_and_bucket_unknown_defaults_to_day() {
        assert_eq!(
            period_to_interval_and_bucket("garbage"),
            ("-1 day", "%Y-%m-%dT%H:00:00")
        );
    }

    #[test]
    fn period_to_interval_and_bucket_empty_defaults_to_day() {
        assert_eq!(
            period_to_interval_and_bucket(""),
            ("-1 day", "%Y-%m-%dT%H:00:00")
        );
    }

    // -----------------------------------------------------------------------
    // extract_container_statuses
    // -----------------------------------------------------------------------

    fn make_container(
        labels: Option<HashMap<String, String>>,
        state: Option<ContainerSummaryStateEnum>,
    ) -> ContainerSummary {
        ContainerSummary {
            labels,
            state,
            ..Default::default()
        }
    }

    #[test]
    fn extract_container_statuses_empty_input() {
        let result = extract_container_statuses(vec![], &HashMap::new());
        assert!(result.is_empty());
    }

    #[test]
    fn extract_container_statuses_running_with_labels() {
        let mut labels = HashMap::new();
        labels.insert(
            "sovereign-engine.model-id".to_string(),
            "my-model".to_string(),
        );
        labels.insert("sovereign-engine.backend".to_string(), "vllm".to_string());
        let containers = vec![make_container(
            Some(labels),
            Some(ContainerSummaryStateEnum::RUNNING),
        )];

        let mut vram = HashMap::new();
        vram.insert("my-model".to_string(), 4096u64);

        let statuses = extract_container_statuses(containers, &vram);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].model_id, "my-model");
        assert_eq!(statuses[0].backend_type, "vllm");
        assert!(statuses[0].healthy);
        assert_eq!(statuses[0].state.as_deref(), Some("running"));
        assert_eq!(statuses[0].vram_used_mb, Some(4096));
    }

    #[test]
    fn extract_container_statuses_stopped_without_labels() {
        let containers = vec![make_container(
            None,
            Some(ContainerSummaryStateEnum::EXITED),
        )];

        let statuses = extract_container_statuses(containers, &HashMap::new());
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].model_id, ""); // default
        assert_eq!(statuses[0].backend_type, "llamacpp"); // default
        assert!(!statuses[0].healthy);
        assert_eq!(statuses[0].state.as_deref(), Some("exited"));
        assert_eq!(statuses[0].vram_used_mb, None);
    }

    #[test]
    fn extract_container_statuses_no_state() {
        let containers = vec![make_container(None, None)];
        let statuses = extract_container_statuses(containers, &HashMap::new());
        assert!(!statuses[0].healthy);
        assert!(statuses[0].state.is_none());
    }

    #[test]
    fn extract_container_statuses_vram_absent_for_model() {
        let mut labels = HashMap::new();
        labels.insert(
            "sovereign-engine.model-id".to_string(),
            "model-a".to_string(),
        );
        let containers = vec![make_container(
            Some(labels),
            Some(ContainerSummaryStateEnum::RUNNING),
        )];

        // vram_map has a different model
        let mut vram = HashMap::new();
        vram.insert("model-b".to_string(), 1024u64);

        let statuses = extract_container_statuses(containers, &vram);
        assert_eq!(statuses[0].model_id, "model-a");
        assert_eq!(statuses[0].vram_used_mb, None);
    }

    #[test]
    fn extract_container_statuses_multiple_containers() {
        let mut labels1 = HashMap::new();
        labels1.insert("sovereign-engine.model-id".to_string(), "m1".to_string());
        let mut labels2 = HashMap::new();
        labels2.insert("sovereign-engine.model-id".to_string(), "m2".to_string());

        let containers = vec![
            make_container(Some(labels1), Some(ContainerSummaryStateEnum::RUNNING)),
            make_container(Some(labels2), Some(ContainerSummaryStateEnum::PAUSED)),
        ];

        let statuses = extract_container_statuses(containers, &HashMap::new());
        assert_eq!(statuses.len(), 2);
        assert!(statuses[0].healthy);
        assert!(!statuses[1].healthy);
        assert_eq!(statuses[1].state.as_deref(), Some("paused"));
    }
}
