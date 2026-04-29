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
use crate::docker::runtime_overrides::{
    ModelRuntimeOverrides, PersistedLaunchConfig, WrappedRuntimeJson,
};
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
    /// Companion mmproj filename (bare; no directory prefix). Set by 7b's
    /// download path and by 7a's startup backfill. When `Some`, the container
    /// start path composes `<safe_repo>/<this>` into `LlamacppConfig::mmproj_path`.
    pub mmproj_filename: Option<String>,
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
        "SELECT id, hf_repo, filename, backend_type, context_length, runtime_overrides, mmproj_filename FROM models WHERE id = ?",
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
        mmproj_filename,
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

    // A bad JSON blob in the DB shouldn't keep the model from starting —
    // fall back to defaults (i.e. no overrides) and carry on.
    // `WrappedRuntimeJson::parse` accepts both the new wrapped shape
    // (`{"cli": {...}, "launch": {...}}`) and the legacy bare
    // `ModelRuntimeOverrides` shape — legacy blobs surface as
    // `cli=<legacy>, launch=<default>`. Lifted out of the backend-type match
    // so the post-start write-back can see `wrapped.cli`.
    let wrapped = WrappedRuntimeJson::parse(&runtime_overrides_json).unwrap_or_default();
    let overrides: ModelRuntimeOverrides = wrapped.cli.clone();

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

            // Same `<safe_repo>/<bare>` composition as gguf_path — the column
            // stores the bare filename only. File-existence is re-verified
            // inside build_llamacpp_cmd before the --mmproj flag is emitted.
            let mmproj_path = mmproj_filename
                .as_ref()
                .map(|f| format!("{}/{}", safe_repo, f));

            let parallel = params.parallel.unwrap_or(1).max(1);
            let llamacpp_config = crate::docker::llamacpp::LlamacppConfig {
                model_id: model_id.clone(),
                gguf_path,
                mmproj_path,
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
            // Persist the launch sub-struct so the supervisor can rebuild
            // `LlamacppConfig` on restart without re-prompting the operator.
            // Done BEFORE gate-register / loaded=1 so a crash mid-bookkeeping
            // doesn't leave us with a stale runtime_overrides blob. A failure
            // here is logged and swallowed — the supervisor's legacy-data path
            // (Phase 3) skips models without a launch sub-struct rather than
            // breaking the start.
            let new_wrapped = WrappedRuntimeJson {
                cli: wrapped.cli,
                launch: PersistedLaunchConfig {
                    gpu_type: params.gpu_type.clone(),
                    gpu_layers: params.gpu_layers,
                    parallel: params.parallel,
                },
            };
            match new_wrapped.to_json() {
                Ok(blob) => {
                    if let Err(e) = sqlx::query(
                        "UPDATE models SET runtime_overrides = ? WHERE id = ?",
                    )
                    .bind(&blob)
                    .bind(&model_id)
                    .execute(&state.db.pool)
                    .await
                    {
                        error!(
                            model = %model_id,
                            error = %e,
                            "Failed to persist launch config",
                        );
                    }
                }
                Err(e) => {
                    error!(
                        model = %model_id,
                        error = %e,
                        "Failed to serialise launch config",
                    );
                }
            }

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

            // Phase 4: reset worked on container start. The new container
            // hasn't served a 2xx yet — clear both halves of the hybrid
            // (in-memory atomic + persisted DB column). The hot-path will
            // flip them back to true on the first successful response.
            state
                .worked_map
                .insert(model_id.clone(), std::sync::atomic::AtomicBool::new(false));
            let _ = sqlx::query("UPDATE models SET worked = 0 WHERE id = ?")
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

/// Reconcile state for a backend whose container has died (or was discovered
/// gone at proxy startup). Idempotent.
///
/// Steps:
/// 1. Clear `models.loaded = 0`.
/// 2. Unregister the gate slot (`state.scheduler.gate().unregister(...)`).
/// 3. Drop in-memory state if any (Phase 4 will populate this).
/// 4. Write a row to `backend_crash_log` with the supplied `discovery_reason`
///    (used as the `signal` column for now — Phase 5 adds proper exit-code
///    capture). `container_id` may be `None` when called from the proxy-startup
///    path; `occurred_at` is set by the schema default.
///
/// `discovery_reason` is a free-form short string, e.g.
/// `"discovered_at_proxy_startup"` or `"supervisor_probe_failure"`.
///
/// Best-effort: a DB failure on the crash-log insert is logged and swallowed
/// rather than propagated, since reconciliation should never block recovery.
pub(crate) async fn reconcile_dead_backend(
    state: &Arc<AppState>,
    model_id: &str,
    container_id: Option<&str>,
    discovery_reason: &str,
) {
    // 1. Clear the loaded flag.
    if let Err(e) = sqlx::query("UPDATE models SET loaded = 0 WHERE id = ?")
        .bind(model_id)
        .execute(&state.db.pool)
        .await
    {
        error!(
            model = %model_id,
            error = %e,
            "reconcile_dead_backend: failed to clear loaded flag",
        );
    }

    // 2. Unregister the gate slot.
    state.scheduler.gate().unregister(model_id).await;

    // 3. Drop the in-memory worked-flag entry (Phase 4). A crashed model has
    //    no entry; the next container-start path re-inserts a `false` atomic.
    //    The persisted `models.worked` column is intentionally left intact —
    //    Phase 6's quarantine decision wants to know whether the *previous*
    //    container instance ever served a 2xx, not just the live one.
    state.worked_map.remove(model_id);

    // 4. Append a crash-log row.
    if let Err(e) = sqlx::query(
        "INSERT INTO backend_crash_log (model_id, container_id, signal) VALUES (?, ?, ?)",
    )
    .bind(model_id)
    .bind(container_id)
    .bind(discovery_reason)
    .execute(&state.db.pool)
    .await
    {
        error!(
            model = %model_id,
            error = %e,
            "reconcile_dead_backend: failed to write crash log row",
        );
    }
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

    // -----------------------------------------------------------------------
    // ModelStartRow: the SELECT must carry mmproj_filename through to the
    // struct so start_container_core can wire it into LlamacppConfig.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn model_start_row_reads_mmproj_filename() {
        let db = crate::db::Database::test_db().await;
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, mmproj_filename) \
             VALUES (?, ?, ?, 'llamacpp', 0, ?)",
        )
        .bind("m-with-mmproj")
        .bind("owner/repo")
        .bind("model.gguf")
        .bind("mmproj-f16.gguf")
        .execute(&db.pool)
        .await
        .expect("insert model");

        let row: ModelStartRow = sqlx::query_as(
            "SELECT id, hf_repo, filename, backend_type, context_length, runtime_overrides, mmproj_filename FROM models WHERE id = ?",
        )
        .bind("m-with-mmproj")
        .fetch_one(&db.pool)
        .await
        .expect("fetch row");

        assert_eq!(row.mmproj_filename.as_deref(), Some("mmproj-f16.gguf"));
    }

    #[tokio::test]
    async fn model_start_row_mmproj_filename_is_none_for_text_only() {
        let db = crate::db::Database::test_db().await;
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded) \
             VALUES (?, ?, ?, 'llamacpp', 0)",
        )
        .bind("m-text-only")
        .bind("owner/repo")
        .bind("model.gguf")
        .execute(&db.pool)
        .await
        .expect("insert model");

        let row: ModelStartRow = sqlx::query_as(
            "SELECT id, hf_repo, filename, backend_type, context_length, runtime_overrides, mmproj_filename FROM models WHERE id = ?",
        )
        .bind("m-text-only")
        .fetch_one(&db.pool)
        .await
        .expect("fetch row");

        assert!(row.mmproj_filename.is_none());
    }

    // -----------------------------------------------------------------------
    // reconcile_dead_backend
    //
    // Verifies the Phase-2 helper clears the loaded flag, unregisters the gate
    // slot, and writes a single backend_crash_log row carrying the supplied
    // `discovery_reason` in the `signal` column.
    // -----------------------------------------------------------------------

    fn test_config_for_reconcile() -> crate::config::AppConfig {
        crate::config::AppConfig {
            listen_addr: "127.0.0.1:0".to_string(),
            database_url: "sqlite::memory:".to_string(),
            tls_cert_path: None,
            tls_key_path: None,
            bootstrap_user: None,
            bootstrap_password: None,
            break_glass: false,
            docker_host: "unix:///var/run/docker.sock".to_string(),
            model_path: "/tmp/test-models-reconcile".to_string(),
            model_host_path: "/tmp/test-models-reconcile".to_string(),
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

    async fn build_test_state() -> Arc<AppState> {
        let db = crate::db::Database::test_db().await;
        let (probe_tx, _probe_rx) = crate::supervisor::channel();
        Arc::new(AppState {
            config: test_config_for_reconcile(),
            db,
            docker: crate::docker::DockerManager::test_dummy(),
            scheduler: crate::scheduler::Scheduler::new(),
            metrics: crate::metrics::MetricsBroadcaster::new(),
            reservations: crate::scheduler::reservation::ReservationBroadcaster::new(),
            supervisor_map: std::sync::Arc::new(dashmap::DashMap::new()),
            probe_tx,
            worked_map: std::sync::Arc::new(dashmap::DashMap::new()),
        })
    }

    #[tokio::test]
    async fn reconcile_dead_backend_clears_loaded_and_writes_crash_row() {
        let state = build_test_state().await;
        let model_id = "m-reconcile";

        // Seed: a loaded model row.
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, runtime_overrides) \
             VALUES (?, ?, ?, 'llamacpp', 1, '{}')",
        )
        .bind(model_id)
        .bind("owner/repo")
        .bind("model.gguf")
        .execute(&state.db.pool)
        .await
        .expect("insert model");

        // Pre-register a gate slot so we can confirm reconcile clears it.
        state.scheduler.gate().register(model_id, 2).await;
        let snapshot_before = state.scheduler.gate().status().await;
        assert!(snapshot_before.contains_key(model_id));

        // Act
        super::reconcile_dead_backend(
            &state,
            model_id,
            Some("container-id-abc"),
            "test_reason",
        )
        .await;

        // Assert: loaded cleared.
        let loaded: i64 = sqlx::query_scalar("SELECT loaded FROM models WHERE id = ?")
            .bind(model_id)
            .fetch_one(&state.db.pool)
            .await
            .expect("query loaded");
        assert_eq!(loaded, 0, "loaded should be cleared");

        // Assert: gate slot removed.
        let snapshot_after = state.scheduler.gate().status().await;
        assert!(
            !snapshot_after.contains_key(model_id),
            "gate slot should be unregistered"
        );

        // Assert: exactly one crash log row, with signal=test_reason.
        let crash_rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT model_id, container_id, signal FROM backend_crash_log WHERE model_id = ?",
        )
        .bind(model_id)
        .fetch_all(&state.db.pool)
        .await
        .expect("query crash log");

        assert_eq!(crash_rows.len(), 1, "expected exactly one crash log row");
        let (logged_model, logged_container, logged_signal) = &crash_rows[0];
        assert_eq!(logged_model, model_id);
        assert_eq!(logged_container.as_deref(), Some("container-id-abc"));
        assert_eq!(logged_signal.as_deref(), Some("test_reason"));
    }

    #[tokio::test]
    async fn reconcile_dead_backend_handles_missing_container_id() {
        let state = build_test_state().await;
        let model_id = "m-no-container";

        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, runtime_overrides) \
             VALUES (?, ?, ?, 'llamacpp', 1, '{}')",
        )
        .bind(model_id)
        .bind("owner/repo")
        .bind("model.gguf")
        .execute(&state.db.pool)
        .await
        .expect("insert model");

        // No prior gate registration — reconcile must still be idempotent.
        super::reconcile_dead_backend(
            &state,
            model_id,
            None,
            "discovered_at_proxy_startup",
        )
        .await;

        let loaded: i64 = sqlx::query_scalar("SELECT loaded FROM models WHERE id = ?")
            .bind(model_id)
            .fetch_one(&state.db.pool)
            .await
            .expect("query loaded");
        assert_eq!(loaded, 0);

        let crash: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT container_id, signal FROM backend_crash_log WHERE model_id = ?",
        )
        .bind(model_id)
        .fetch_one(&state.db.pool)
        .await
        .expect("query crash log");
        assert!(crash.0.is_none(), "container_id should be NULL");
        assert_eq!(crash.1.as_deref(), Some("discovered_at_proxy_startup"));
    }
}
