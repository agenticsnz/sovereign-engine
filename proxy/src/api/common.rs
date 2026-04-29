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
use crate::metrics::{ContainerStatus, LastCrashSummary};
use crate::supervisor::BackendFsmState;
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
        "SELECT id, hf_repo, filename, size_bytes, category_id, loaded, backend_port, backend_type, last_used_at, created_at, context_length, n_layers, n_heads, n_kv_heads, embedding_length, key_length, value_length, sliding_window, kv_bytes_per_token_global, kv_bytes_per_token_swa, mmproj_filename, runtime_overrides, quarantined_at, quarantine_reason FROM models",
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
                // Phase 7: enriched fields are populated by
                // `extract_container_statuses_enriched`. The pure helper
                // leaves them at their None/false defaults.
                fsm_state: None,
                quarantined: false,
                quarantine_reason: None,
                last_crash: None,
            }
        })
        .collect()
}

/// Pretty name for an [`BackendFsmState`] — what the UI renders in the badge.
fn fsm_state_label(s: BackendFsmState) -> &'static str {
    match s {
        BackendFsmState::Starting => "Starting",
        BackendFsmState::Healthy => "Healthy",
        BackendFsmState::Suspect => "Suspect",
        BackendFsmState::Crashed => "Crashed",
        BackendFsmState::Quarantined => "Quarantined",
    }
}

/// Phase 7 enriched variant of [`extract_container_statuses`].
///
/// Adds the supervisor FSM label, quarantine flag/reason, and a one-line
/// summary of the most-recent crash event. One DB round-trip per model
/// (acceptable for an admin page; max ~tens of models per box).
///
/// Implemented as a wrapper around the pure helper so the existing test
/// suite stays intact and the sync version remains usable from contexts
/// without DB access (none today, but keep the option open).
pub async fn extract_container_statuses_enriched(
    state: &Arc<AppState>,
    containers: Vec<bollard::models::ContainerSummary>,
    vram_map: &std::collections::HashMap<String, u64>,
) -> Vec<ContainerStatus> {
    let mut base = extract_container_statuses(containers, vram_map);

    for status in base.iter_mut() {
        if status.model_id.is_empty() {
            continue;
        }

        // FSM state — direct read from the in-memory supervisor map. No
        // DB cost.
        if let Some(entry) = state.supervisor_map.get(&status.model_id) {
            status.fsm_state = Some(fsm_state_label(entry.fsm).to_string());
        }

        // Quarantine columns. Best-effort; on DB error we leave defaults
        // and log so the page still renders.
        match sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "SELECT quarantined_at, quarantine_reason FROM models WHERE id = ?",
        )
        .bind(&status.model_id)
        .fetch_optional(&state.db.pool)
        .await
        {
            Ok(Some((quar_at, quar_reason))) => {
                status.quarantined = quar_at.is_some();
                status.quarantine_reason = quar_reason;
            }
            Ok(None) => {
                // Container exists but no models row — leave defaults.
            }
            Err(e) => {
                error!(
                    model = %status.model_id,
                    error = %e,
                    "extract_container_statuses_enriched: failed to read quarantine columns",
                );
            }
        }

        // Most-recent crash row.
        match sqlx::query_as::<_, (String, Option<i64>, i64, Option<String>)>(
            "SELECT occurred_at, exit_code, oom_killed, log_path \
             FROM backend_crash_log WHERE model_id = ? \
             ORDER BY occurred_at DESC LIMIT 1",
        )
        .bind(&status.model_id)
        .fetch_optional(&state.db.pool)
        .await
        {
            Ok(Some((occurred_at, exit_code, oom_killed, log_path))) => {
                status.last_crash = Some(LastCrashSummary {
                    occurred_at,
                    exit_code,
                    oom_killed: oom_killed != 0,
                    log_path_present: log_path.is_some(),
                });
            }
            Ok(None) => {
                // No crash history yet — leave None.
            }
            Err(e) => {
                error!(
                    model = %status.model_id,
                    error = %e,
                    "extract_container_statuses_enriched: failed to read last crash",
                );
            }
        }
    }

    base
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

            // Phase 6: a manual restart doubles as un-quarantine. Per
            // decision 4 (note 019dd7f3-…, 2026-04-29), anyone with
            // permission to start a model can rescue a quarantined one —
            // no dedicated unquarantine endpoint. The UPDATE on a
            // non-quarantined row is a no-op, so we don't gate on a SELECT.
            clear_quarantine(state, &model_id).await;

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
    reconcile_state(state, model_id).await;

    // Append a basic crash-log row. Phase 5's enriched variant
    // [`reconcile_dead_backend_with_capture`] persists exit_code, oom_killed,
    // and a log_path on top of what's written here.
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

/// Shared "clear loaded, unregister gate, drop worked entry" reconciliation
/// steps. Used by both [`reconcile_dead_backend`] (basic) and
/// [`reconcile_dead_backend_with_capture`] (Phase 5 enriched) before each
/// writes its own `backend_crash_log` INSERT.
///
/// Idempotent and best-effort.
pub(crate) async fn reconcile_state(state: &Arc<AppState>, model_id: &str) {
    // 1. Clear the loaded flag.
    if let Err(e) = sqlx::query("UPDATE models SET loaded = 0 WHERE id = ?")
        .bind(model_id)
        .execute(&state.db.pool)
        .await
    {
        error!(
            model = %model_id,
            error = %e,
            "reconcile_state: failed to clear loaded flag",
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
}

/// Phase 5 enriched variant of [`reconcile_dead_backend`].
///
/// Persists captured crash diagnostics — `exit_code`, `oom_killed`, `signal`,
/// `log_path` — into the `backend_crash_log` row, in addition to the
/// shared "clear loaded / unregister gate / drop worked entry" steps.
///
/// **Caller is responsible for capturing diagnostics via
/// [`crate::supervisor::capture_crash_state`] BEFORE calling this** — by the
/// time this function returns, [`crate::supervisor`] (Phase 6) may proceed
/// to remove and recreate the container, at which point Docker discards the
/// container's logs.
///
/// `signal` precedence: if the inspect call surfaced a non-empty
/// `state.error` string (`capture.signal`), that value wins; otherwise we
/// fall back to the supervisor's `discovery_reason` so the row never has a
/// NULL signal column.
///
/// Best-effort: a DB failure on the INSERT is logged and swallowed.
pub(crate) async fn reconcile_dead_backend_with_capture(
    state: &Arc<AppState>,
    model_id: &str,
    capture: &crate::supervisor::CrashCapture,
    log_path: Option<&std::path::Path>,
    discovery_reason: &str,
) {
    reconcile_state(state, model_id).await;

    let signal = capture
        .signal
        .clone()
        .unwrap_or_else(|| discovery_reason.to_string());
    let oom: i64 = if capture.oom_killed { 1 } else { 0 };
    let log_path_str = log_path.map(|p| p.to_string_lossy().to_string());

    if let Err(e) = sqlx::query(
        "INSERT INTO backend_crash_log \
         (model_id, container_id, exit_code, oom_killed, signal, log_path) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(model_id)
    .bind(capture.container_id.as_deref())
    .bind(capture.exit_code)
    .bind(oom)
    .bind(&signal)
    .bind(log_path_str.as_deref())
    .execute(&state.db.pool)
    .await
    {
        error!(
            model = %model_id,
            error = %e,
            "reconcile_dead_backend_with_capture: failed to write crash log row",
        );
    }
}

/// Clear quarantine state for a model (Phase 6).
///
/// A successful manual restart via `start_container_core` is the un-quarantine
/// signal — we don't ship a dedicated `POST .../unquarantine` endpoint
/// (decision 4). Best-effort: a DB failure is logged and swallowed since the
/// container is already starting.
pub(crate) async fn clear_quarantine(state: &Arc<AppState>, model_id: &str) {
    if let Err(e) = sqlx::query(
        "UPDATE models SET quarantined_at = NULL, quarantine_reason = NULL WHERE id = ?",
    )
    .bind(model_id)
    .execute(&state.db.pool)
    .await
    {
        error!(
            model = %model_id,
            error = %e,
            "clear_quarantine: failed to clear quarantine columns",
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
        // Phase 7: pure helper leaves enriched fields at defaults.
        assert!(statuses[0].fsm_state.is_none());
        assert!(!statuses[0].quarantined);
        assert!(statuses[0].quarantine_reason.is_none());
        assert!(statuses[0].last_crash.is_none());
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
            data_path: "/tmp/test-data-path".to_string(),
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

    // -----------------------------------------------------------------------
    // reconcile_dead_backend_with_capture (Phase 5)
    //
    // Verifies the enriched variant persists exit_code, oom_killed, signal,
    // and log_path captured from bollard's inspect into `backend_crash_log`.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reconcile_dead_backend_with_capture_writes_full_row() {
        let state = build_test_state().await;
        let model_id = "m-capture";

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

        // Pre-register a gate slot to confirm shared reconcile_state still
        // unregisters it from the enriched path too.
        state.scheduler.gate().register(model_id, 4).await;

        // Build a fixed-content CrashCapture to assert we round-trip every
        // field into the row.
        let capture = crate::supervisor::CrashCapture {
            container_id: Some("container-deadbeef".to_string()),
            exit_code: Some(137),
            oom_killed: true,
            finished_at: Some("2026-04-29T07:00:00Z".to_string()),
            signal: Some("OOMKilled".to_string()),
            log_tail: b"some log bytes".to_vec(),
        };
        let log_path =
            std::path::PathBuf::from("/config/crash_logs/m-capture-1714377600.log");

        super::reconcile_dead_backend_with_capture(
            &state,
            model_id,
            &capture,
            Some(&log_path),
            "supervisor_probe_failure",
        )
        .await;

        // loaded cleared.
        let loaded: i64 = sqlx::query_scalar("SELECT loaded FROM models WHERE id = ?")
            .bind(model_id)
            .fetch_one(&state.db.pool)
            .await
            .expect("query loaded");
        assert_eq!(loaded, 0);

        // gate unregistered.
        let snapshot = state.scheduler.gate().status().await;
        assert!(!snapshot.contains_key(model_id));

        // exactly one row, all fields populated from capture.
        let row: (
            String,
            Option<String>,
            Option<i64>,
            i64,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT model_id, container_id, exit_code, oom_killed, signal, log_path \
             FROM backend_crash_log WHERE model_id = ?",
        )
        .bind(model_id)
        .fetch_one(&state.db.pool)
        .await
        .expect("query crash log");
        assert_eq!(row.0, model_id);
        assert_eq!(row.1.as_deref(), Some("container-deadbeef"));
        assert_eq!(row.2, Some(137));
        assert_eq!(row.3, 1, "oom_killed should be 1");
        assert_eq!(row.4.as_deref(), Some("OOMKilled"));
        assert_eq!(
            row.5.as_deref(),
            Some("/config/crash_logs/m-capture-1714377600.log")
        );
    }

    // -----------------------------------------------------------------------
    // clear_quarantine (Phase 6)
    //
    // A successful manual restart via start_container_core's success branch
    // calls clear_quarantine — the Start button doubles as un-quarantine.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn clear_quarantine_clears_columns_when_set() {
        let state = build_test_state().await;
        let model_id = "m-clear-quar";

        // Pre-populate model with quarantine state set.
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, runtime_overrides, quarantined_at, quarantine_reason) \
             VALUES (?, ?, ?, 'llamacpp', 0, '{}', ?, ?)",
        )
        .bind(model_id)
        .bind("owner/repo")
        .bind("model.gguf")
        .bind("2026-04-29T12:00:00Z")
        .bind("test reason")
        .execute(&state.db.pool)
        .await
        .expect("insert model");

        super::clear_quarantine(&state, model_id).await;

        let row: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT quarantined_at, quarantine_reason FROM models WHERE id = ?",
        )
        .bind(model_id)
        .fetch_one(&state.db.pool)
        .await
        .expect("query");
        assert!(row.0.is_none(), "quarantined_at should be cleared");
        assert!(row.1.is_none(), "quarantine_reason should be cleared");
    }

    #[tokio::test]
    async fn clear_quarantine_is_noop_for_unquarantined_model() {
        let state = build_test_state().await;
        let model_id = "m-not-quar";

        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, runtime_overrides) \
             VALUES (?, ?, ?, 'llamacpp', 0, '{}')",
        )
        .bind(model_id)
        .bind("owner/repo")
        .bind("model.gguf")
        .execute(&state.db.pool)
        .await
        .expect("insert model");

        super::clear_quarantine(&state, model_id).await;

        // Row still exists, columns still NULL.
        let row: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT quarantined_at, quarantine_reason FROM models WHERE id = ?",
        )
        .bind(model_id)
        .fetch_one(&state.db.pool)
        .await
        .expect("query");
        assert!(row.0.is_none());
        assert!(row.1.is_none());
    }

    #[tokio::test]
    async fn reconcile_dead_backend_with_capture_falls_back_to_discovery_reason_for_signal() {
        let state = build_test_state().await;
        let model_id = "m-no-signal";

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

        // capture.signal = None — discovery_reason should be persisted.
        let capture = crate::supervisor::CrashCapture {
            container_id: None,
            exit_code: None,
            oom_killed: false,
            finished_at: None,
            signal: None,
            log_tail: Vec::new(),
        };

        super::reconcile_dead_backend_with_capture(
            &state,
            model_id,
            &capture,
            None,
            "supervisor_probe_failure",
        )
        .await;

        let row: (Option<String>, i64, Option<String>) = sqlx::query_as(
            "SELECT signal, oom_killed, log_path \
             FROM backend_crash_log WHERE model_id = ?",
        )
        .bind(model_id)
        .fetch_one(&state.db.pool)
        .await
        .expect("query crash log");
        assert_eq!(row.0.as_deref(), Some("supervisor_probe_failure"));
        assert_eq!(row.1, 0);
        assert!(row.2.is_none());
    }

    // -----------------------------------------------------------------------
    // extract_container_statuses_enriched (Phase 7)
    //
    // Verifies the enriched wrapper merges supervisor_map FSM state, models
    // quarantine columns, and the most-recent backend_crash_log row into the
    // base ContainerStatus.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn extract_container_statuses_enriched_populates_fsm_quarantine_and_last_crash() {
        let state = build_test_state().await;
        let model_id = "m-enriched";

        // Seed the model row, quarantined.
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, runtime_overrides, quarantined_at, quarantine_reason) \
             VALUES (?, ?, ?, 'llamacpp', 1, '{}', ?, ?)",
        )
        .bind(model_id)
        .bind("owner/repo")
        .bind("model.gguf")
        .bind("2026-04-29T11:00:00Z")
        .bind("test quarantine")
        .execute(&state.db.pool)
        .await
        .expect("insert model");

        // Two crash rows — the most recent should win.
        sqlx::query(
            "INSERT INTO backend_crash_log (model_id, occurred_at, exit_code, oom_killed, signal, log_path) \
             VALUES (?, '2026-04-29T10:00:00Z', 1, 0, 'older', NULL)",
        )
        .bind(model_id)
        .execute(&state.db.pool)
        .await
        .expect("older crash row");

        sqlx::query(
            "INSERT INTO backend_crash_log (model_id, occurred_at, exit_code, oom_killed, signal, log_path) \
             VALUES (?, '2026-04-29T11:00:00Z', 137, 1, 'OOMKilled', '/x/y.log')",
        )
        .bind(model_id)
        .execute(&state.db.pool)
        .await
        .expect("newer crash row");

        // Plant an FSM state in the supervisor map.
        state.supervisor_map.insert(
            model_id.to_string(),
            crate::supervisor::BackendState {
                fsm: crate::supervisor::BackendFsmState::Crashed,
                consecutive_failures: 2,
                started_at: std::time::Instant::now(),
            },
        );

        // Build a fake container summary that matches.
        let mut labels = std::collections::HashMap::new();
        labels.insert(
            "sovereign-engine.model-id".to_string(),
            model_id.to_string(),
        );
        let containers = vec![bollard::models::ContainerSummary {
            labels: Some(labels),
            state: Some(bollard::models::ContainerSummaryStateEnum::EXITED),
            ..Default::default()
        }];

        let result = super::extract_container_statuses_enriched(
            &state,
            containers,
            &std::collections::HashMap::new(),
        )
        .await;

        assert_eq!(result.len(), 1);
        let s = &result[0];
        assert_eq!(s.model_id, model_id);
        assert_eq!(s.fsm_state.as_deref(), Some("Crashed"));
        assert!(s.quarantined);
        assert_eq!(s.quarantine_reason.as_deref(), Some("test quarantine"));
        let last = s.last_crash.as_ref().expect("last_crash present");
        assert_eq!(last.occurred_at, "2026-04-29T11:00:00Z");
        assert_eq!(last.exit_code, Some(137));
        assert!(last.oom_killed);
        assert!(last.log_path_present);
    }

    #[tokio::test]
    async fn extract_container_statuses_enriched_leaves_defaults_when_no_data() {
        let state = build_test_state().await;
        let model_id = "m-bare";

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

        let mut labels = std::collections::HashMap::new();
        labels.insert(
            "sovereign-engine.model-id".to_string(),
            model_id.to_string(),
        );
        let containers = vec![bollard::models::ContainerSummary {
            labels: Some(labels),
            state: Some(bollard::models::ContainerSummaryStateEnum::RUNNING),
            ..Default::default()
        }];

        let result = super::extract_container_statuses_enriched(
            &state,
            containers,
            &std::collections::HashMap::new(),
        )
        .await;

        let s = &result[0];
        assert!(s.fsm_state.is_none(), "no supervisor entry → no fsm_state");
        assert!(!s.quarantined);
        assert!(s.quarantine_reason.is_none());
        assert!(s.last_crash.is_none());
    }
}
