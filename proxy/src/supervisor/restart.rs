//! Phase 6 — restart-from-persisted-state and quarantine helpers.
//!
//! When the supervisor flips a backend to `Crashed` (Phase 5 already captured
//! diagnostics, wrote the log file, reconciled state, and GC'd old crash
//! logs), this module decides what to do next based on the
//! [`crate::supervisor::read_worked`] flag:
//!
//! - `worked == true`  → call [`restart_after_crash`] to rebuild
//!   `LlamacppConfig` from the persisted `models` row + `runtime_overrides`
//!   blob + `container_secrets` row, then re-launch via
//!   `start_llamacpp`. On failure, the caller falls back to
//!   [`quarantine_model`].
//! - `worked == false` → call [`quarantine_model`] directly.
//!
//! Quarantine is purely "do not auto-restart" (decision 4 on note
//! `019dd7f3-…`). A quarantined model is reachable through the normal admin /
//! reservation Start path — that path doubles as un-quarantine because
//! `start_container_core` clears `quarantined_at` / `quarantine_reason` as a
//! side effect.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use sqlx::FromRow;
use tracing::{error, info, warn};

use crate::docker::llamacpp::{GpuType, LlamacppConfig};
use crate::docker::runtime_overrides::WrappedRuntimeJson;

use super::{BackendFsmState, BackendState};

// ---------------------------------------------------------------------------
// Persisted-row shapes
// ---------------------------------------------------------------------------

/// Row from `models` carrying everything the restart path needs. Mirrors
/// `crate::api::common::ModelStartRow` but lives here to keep this module
/// self-contained.
#[derive(FromRow, Debug, Clone)]
pub(crate) struct RestartModelRow {
    pub id: String,
    pub hf_repo: String,
    pub filename: Option<String>,
    pub backend_type: String,
    pub context_length: Option<i64>,
    pub runtime_overrides: String,
    pub mmproj_filename: Option<String>,
}

/// Row from `container_secrets` carrying the persisted UID + API key the
/// restart path MUST reuse so existing API-key-holding clients keep working.
#[derive(FromRow, Debug, Clone)]
pub(crate) struct RestartSecretsRow {
    pub container_uid: i64,
    pub api_key: String,
    pub parallel_slots: i64,
}

// ---------------------------------------------------------------------------
// Pure helper — rebuild `LlamacppConfig` from persisted rows
// ---------------------------------------------------------------------------

/// Pure function: rebuild a [`LlamacppConfig`] from the persisted model row,
/// container-secrets row, and parsed runtime-overrides wrapper.
///
/// No I/O — extracted so it can be unit-tested without a Docker daemon. The
/// surrounding orchestration ([`restart_after_crash`]) handles the SELECTs
/// and the `start_llamacpp` call.
///
/// Returns `Err(reason)` when:
/// - `filename` is `None` (no GGUF file recorded — operator must fix forward)
/// - `context_length` is `None` (no context size recorded)
/// - the launch sub-struct is fully default (legacy data — operator must
///   restart manually so they choose gpu_type / parallel)
pub(crate) fn rebuild_llamacpp_config(
    model_row: &RestartModelRow,
    secrets_row: &RestartSecretsRow,
    wrapped: &WrappedRuntimeJson,
) -> Result<LlamacppConfig, String> {
    let filename = model_row.filename.as_deref().ok_or_else(|| {
        format!(
            "model {} has no filename recorded — cannot determine GGUF path",
            model_row.id
        )
    })?;
    let context_length = model_row.context_length.ok_or_else(|| {
        format!(
            "model {} has no context_length recorded — cannot restart",
            model_row.id
        )
    })?;

    // Legacy data check — if the launch sub-struct never got populated by a
    // post-Phase-1 start, we don't know the operator's gpu_type / parallel
    // choice. Refuse rather than guessing.
    let launch = &wrapped.launch;
    if launch.gpu_type.is_none() && launch.gpu_layers.is_none() && launch.parallel.is_none() {
        return Err(format!(
            "legacy runtime_overrides for model {} — operator must restart manually",
            model_row.id
        ));
    }

    let safe_repo = model_row.hf_repo.replace('/', "--");
    let gguf_path = format!("{safe_repo}/{filename}");
    let mmproj_path = model_row
        .mmproj_filename
        .as_ref()
        .map(|f| format!("{safe_repo}/{f}"));

    let parallel = launch.parallel.unwrap_or(1).max(1);
    let gpu_layers = launch.gpu_layers.unwrap_or(99);
    let gpu_type = GpuType::from_str(launch.gpu_type.as_deref().unwrap_or("none"));

    Ok(LlamacppConfig {
        model_id: model_row.id.clone(),
        gguf_path,
        mmproj_path,
        gpu_type,
        gpu_layers,
        context_size: context_length as u32,
        parallel,
        extra_args: wrapped.cli.to_cli_args(),
        // CRITICAL: reuse persisted UID + api_key so existing clients
        // continue to work after restart. Do NOT call `allocate_uid()`.
        uid: secrets_row.container_uid as u32,
        api_key: secrets_row.api_key.clone(),
    })
}

// ---------------------------------------------------------------------------
// Restart-from-persisted-state
// ---------------------------------------------------------------------------

/// Restart a crashed backend by reconstructing [`LlamacppConfig`] from the
/// persisted state in the DB (models row + runtime_overrides JSON +
/// container_secrets row) and calling `start_llamacpp`.
///
/// **Reuses the existing `api_key` and `container_uid`** from
/// `container_secrets` — clients that hold the api_key continue to work
/// without any rotation.
///
/// On success: re-registers the gate, sets `loaded = 1`, resets `worked = 0`
/// (in-memory + DB), seeds the supervisor map with a fresh `Healthy` state.
///
/// On failure: returns `Err(reason)`. The caller is responsible for
/// quarantining (we don't quarantine here so the same helper can be called
/// from explicit operator-driven retries in the future without forcing a
/// quarantine on failure). `loaded=0` was already cleared by Phase 5's
/// `reconcile_dead_backend_with_capture`, so no extra cleanup is needed.
pub async fn restart_after_crash(
    state: &Arc<crate::AppState>,
    model_id: &str,
) -> Result<(), String> {
    // 1. Look up the model row.
    let model_row: RestartModelRow = sqlx::query_as(
        "SELECT id, hf_repo, filename, backend_type, context_length, runtime_overrides, mmproj_filename \
         FROM models WHERE id = ?",
    )
    .bind(model_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| format!("DB error fetching model row: {e}"))?
    .ok_or_else(|| format!("missing persisted state for restart: model {model_id} not found"))?;

    // 2. Look up the container_secrets row — the persisted UID + api_key.
    let secrets_row: RestartSecretsRow = sqlx::query_as(
        "SELECT container_uid, api_key, parallel_slots FROM container_secrets WHERE model_id = ?",
    )
    .bind(model_id)
    .fetch_optional(&state.db.pool)
    .await
    .map_err(|e| format!("DB error fetching container_secrets: {e}"))?
    .ok_or_else(|| {
        format!("missing persisted state for restart: container_secrets row missing for {model_id}")
    })?;

    // 3. Parse the wrapped runtime_overrides blob.
    let wrapped = WrappedRuntimeJson::parse(&model_row.runtime_overrides)
        .map_err(|e| format!("failed to parse runtime_overrides for {model_id}: {e}"))?;

    // 4. Backend type — only llamacpp is supported. Anything else → caller
    //    quarantines.
    if model_row.backend_type != "llamacpp" {
        return Err(format!(
            "unsupported backend_type for restart: {}",
            model_row.backend_type
        ));
    }

    // 5. Rebuild the config (pure helper — also performs the legacy-data check).
    let config = rebuild_llamacpp_config(&model_row, &secrets_row, &wrapped)?;

    // 6. Launch the container. `start_llamacpp` removes any existing stopped
    //    container before starting fresh — Phase 5 already captured the
    //    crash logs at this point, so the SAFETY invariant from
    //    `handle_kick` is preserved.
    let container_name = state
        .docker
        .start_llamacpp(&config)
        .await
        .map_err(|e| format!("start_llamacpp failed: {e:#}"))?;

    info!(
        model = %model_id,
        container = %container_name,
        "Backend restarted from persisted state",
    );

    // 7. Post-start bookkeeping. Mirrors the success branch of
    //    `start_container_core` minus the secrets INSERT (we reused the
    //    existing row) and the runtime_overrides write-back (no operator
    //    input changed).
    state
        .worked_map
        .insert(model_id.to_string(), AtomicBool::new(false));
    if let Err(e) = sqlx::query("UPDATE models SET worked = 0 WHERE id = ?")
        .bind(model_id)
        .execute(&state.db.pool)
        .await
    {
        error!(
            model = %model_id,
            error = %e,
            "restart_after_crash: failed to reset worked column",
        );
    }

    state
        .scheduler
        .gate()
        .register(model_id, secrets_row.parallel_slots.max(1) as u32)
        .await;

    if let Err(e) = sqlx::query("UPDATE models SET loaded = 1 WHERE id = ?")
        .bind(model_id)
        .execute(&state.db.pool)
        .await
    {
        error!(
            model = %model_id,
            error = %e,
            "restart_after_crash: failed to set loaded=1",
        );
    }

    // 8. Seed supervisor map with a fresh Healthy entry. The backend may
    //    still be loading the model — `started_at = now` puts us in the
    //    5-minute startup grace if the next probe sees a 503/transport
    //    failure.
    state.supervisor_map.insert(
        model_id.to_string(),
        BackendState {
            fsm: BackendFsmState::Healthy,
            consecutive_failures: 0,
            started_at: Instant::now(),
        },
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Quarantine helper
// ---------------------------------------------------------------------------

/// Persist quarantine state for `model_id` and mark the supervisor map entry
/// as `Quarantined` for visibility.
///
/// Best-effort: a DB failure is logged but not propagated (the supervisor
/// already cleared `loaded=0`, so the existing routing path returns
/// `backend_unavailable` regardless of whether the quarantine columns made
/// it to disk).
pub async fn quarantine_model(state: &Arc<crate::AppState>, model_id: &str, reason: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    if let Err(e) =
        sqlx::query("UPDATE models SET quarantined_at = ?, quarantine_reason = ? WHERE id = ?")
            .bind(&now)
            .bind(reason)
            .bind(model_id)
            .execute(&state.db.pool)
            .await
    {
        error!(
            model = %model_id,
            error = %e,
            "quarantine_model: failed to write quarantine columns",
        );
    } else {
        warn!(model = %model_id, %reason, "Backend quarantined");
    }

    // Update supervisor map FSM to Quarantined for visibility (Phase 7 admin
    // UI surfaces this).
    if let Some(mut entry) = state.supervisor_map.get_mut(model_id) {
        entry.fsm = BackendFsmState::Quarantined;
    }
}

// ---------------------------------------------------------------------------
// Decision helper — testable post-crash branch
// ---------------------------------------------------------------------------

/// Outcome of [`decide_post_crash`]. The caller drives the corresponding
/// async work — separating the decision from the I/O makes both halves easy
/// to unit-test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostCrashAction {
    /// `worked == true`: try to auto-restart. On failure, fall back to
    /// quarantine (with the failure reason).
    Restart,
    /// `worked == false`: quarantine immediately. No restart attempt.
    QuarantineNeverWorked,
}

/// Pure decision: given the worked flag, return the post-crash action.
pub(crate) fn decide_post_crash(worked: bool) -> PostCrashAction {
    if worked {
        PostCrashAction::Restart
    } else {
        PostCrashAction::QuarantineNeverWorked
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::runtime_overrides::{
        ModelRuntimeOverrides, PersistedLaunchConfig, WrappedRuntimeJson,
    };
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // rebuild_llamacpp_config (pure helper)
    // -----------------------------------------------------------------------

    fn full_model_row() -> RestartModelRow {
        RestartModelRow {
            id: "m1".to_string(),
            hf_repo: "owner/repo".to_string(),
            filename: Some("model.gguf".to_string()),
            backend_type: "llamacpp".to_string(),
            context_length: Some(8192),
            runtime_overrides: "{}".to_string(),
            mmproj_filename: None,
        }
    }

    fn full_secrets_row() -> RestartSecretsRow {
        RestartSecretsRow {
            container_uid: 12345,
            api_key: "preserved-api-key".to_string(),
            parallel_slots: 2,
        }
    }

    fn populated_wrapped() -> WrappedRuntimeJson {
        WrappedRuntimeJson {
            cli: ModelRuntimeOverrides {
                cache_ram_mib: Some(0),
                ..Default::default()
            },
            launch: PersistedLaunchConfig {
                gpu_type: Some("vulkan".to_string()),
                gpu_layers: Some(50),
                parallel: Some(2),
            },
        }
    }

    #[test]
    fn rebuild_llamacpp_config_full_inputs_round_trip() {
        let model = full_model_row();
        let secrets = full_secrets_row();
        let wrapped = populated_wrapped();
        let cfg = rebuild_llamacpp_config(&model, &secrets, &wrapped).expect("rebuild ok");
        assert_eq!(cfg.model_id, "m1");
        assert_eq!(cfg.gguf_path, "owner--repo/model.gguf");
        assert_eq!(cfg.context_size, 8192);
        assert_eq!(cfg.parallel, 2);
        assert_eq!(cfg.gpu_layers, 50);
        // Persisted UID + api_key reused, NOT regenerated.
        assert_eq!(cfg.uid, 12345);
        assert_eq!(cfg.api_key, "preserved-api-key");
        assert!(matches!(cfg.gpu_type, GpuType::Vulkan));
        // CLI overrides flow through.
        assert!(cfg.extra_args.iter().any(|s| s == "--cache-ram"));
    }

    #[test]
    fn rebuild_llamacpp_config_with_mmproj_composes_safe_repo_path() {
        let mut model = full_model_row();
        model.mmproj_filename = Some("mmproj-f16.gguf".to_string());
        let secrets = full_secrets_row();
        let wrapped = populated_wrapped();
        let cfg = rebuild_llamacpp_config(&model, &secrets, &wrapped).expect("rebuild ok");
        assert_eq!(
            cfg.mmproj_path.as_deref(),
            Some("owner--repo/mmproj-f16.gguf")
        );
    }

    #[test]
    fn rebuild_llamacpp_config_legacy_runtime_overrides_returns_err() {
        let model = full_model_row();
        let secrets = full_secrets_row();
        // launch sub-struct fully default = legacy data.
        let wrapped = WrappedRuntimeJson::default();
        let err = rebuild_llamacpp_config(&model, &secrets, &wrapped).unwrap_err();
        assert!(
            err.contains("legacy runtime_overrides"),
            "expected legacy-data error, got: {err}"
        );
    }

    #[test]
    fn rebuild_llamacpp_config_missing_filename_returns_err() {
        let mut model = full_model_row();
        model.filename = None;
        let secrets = full_secrets_row();
        let wrapped = populated_wrapped();
        let err = rebuild_llamacpp_config(&model, &secrets, &wrapped).unwrap_err();
        assert!(err.contains("filename"), "got: {err}");
    }

    #[test]
    fn rebuild_llamacpp_config_missing_context_length_returns_err() {
        let mut model = full_model_row();
        model.context_length = None;
        let secrets = full_secrets_row();
        let wrapped = populated_wrapped();
        let err = rebuild_llamacpp_config(&model, &secrets, &wrapped).unwrap_err();
        assert!(err.contains("context_length"), "got: {err}");
    }

    #[test]
    fn rebuild_llamacpp_config_parallel_clamps_to_at_least_one() {
        let model = full_model_row();
        let secrets = full_secrets_row();
        let wrapped = WrappedRuntimeJson {
            cli: ModelRuntimeOverrides::default(),
            launch: PersistedLaunchConfig {
                gpu_type: Some("none".to_string()),
                gpu_layers: Some(0),
                parallel: Some(0), // would clamp to 1
            },
        };
        let cfg = rebuild_llamacpp_config(&model, &secrets, &wrapped).expect("rebuild ok");
        assert_eq!(cfg.parallel, 1);
    }

    // -----------------------------------------------------------------------
    // decide_post_crash (pure decision)
    // -----------------------------------------------------------------------

    #[test]
    fn decide_post_crash_worked_true_returns_restart() {
        assert_eq!(decide_post_crash(true), PostCrashAction::Restart);
    }

    #[test]
    fn decide_post_crash_worked_false_returns_quarantine() {
        assert_eq!(
            decide_post_crash(false),
            PostCrashAction::QuarantineNeverWorked
        );
    }

    // -----------------------------------------------------------------------
    // quarantine_model (DB + supervisor map side effects)
    // -----------------------------------------------------------------------

    async fn build_test_state() -> Arc<crate::AppState> {
        use crate::config::AppConfig;
        use crate::db::Database;
        use crate::docker::DockerManager;
        use crate::metrics::MetricsBroadcaster;
        use crate::scheduler::reservation::ReservationBroadcaster;
        use crate::scheduler::Scheduler;

        let db = Database::test_db().await;
        let (probe_tx, _probe_rx) = crate::supervisor::channel();
        Arc::new(crate::AppState {
            config: AppConfig {
                listen_addr: "127.0.0.1:0".to_string(),
                database_url: "sqlite::memory:".to_string(),
                tls_cert_path: None,
                tls_key_path: None,
                bootstrap_user: None,
                bootstrap_password: None,
                break_glass: false,
                docker_host: "unix:///var/run/docker.sock".to_string(),
                model_path: "/tmp/test-models-restart".to_string(),
                model_host_path: "/tmp/test-models-restart".to_string(),
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
            },
            db,
            docker: DockerManager::test_dummy(),
            scheduler: Scheduler::new(),
            metrics: MetricsBroadcaster::new(),
            reservations: ReservationBroadcaster::new(),
            supervisor_map: Arc::new(dashmap::DashMap::new()),
            probe_tx,
            worked_map: Arc::new(dashmap::DashMap::new()),
        })
    }

    async fn insert_model_row(state: &Arc<crate::AppState>, model_id: &str) {
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
    }

    #[tokio::test]
    async fn quarantine_model_writes_columns() {
        let state = build_test_state().await;
        insert_model_row(&state, "m-quar").await;

        quarantine_model(&state, "m-quar", "no successful response since start").await;

        let row: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT quarantined_at, quarantine_reason FROM models WHERE id = ?")
                .bind("m-quar")
                .fetch_one(&state.db.pool)
                .await
                .expect("query");
        assert!(row.0.is_some(), "quarantined_at should be set");
        assert_eq!(row.1.as_deref(), Some("no successful response since start"));
    }

    #[tokio::test]
    async fn quarantine_model_updates_supervisor_map_fsm() {
        let state = build_test_state().await;
        insert_model_row(&state, "m-fsm").await;

        // Pre-populate map with Crashed entry so we can confirm transition.
        state.supervisor_map.insert(
            "m-fsm".to_string(),
            BackendState {
                fsm: BackendFsmState::Crashed,
                consecutive_failures: 2,
                started_at: Instant::now(),
            },
        );

        quarantine_model(&state, "m-fsm", "test reason").await;

        let entry = state.supervisor_map.get("m-fsm").expect("entry present");
        assert_eq!(entry.fsm, BackendFsmState::Quarantined);
    }

    #[tokio::test]
    async fn quarantine_model_no_supervisor_entry_still_writes_columns() {
        // If the supervisor map entry is gone (e.g. removed by a test
        // helper), the DB write must still happen and not panic.
        let state = build_test_state().await;
        insert_model_row(&state, "m-noentry").await;

        quarantine_model(&state, "m-noentry", "reason").await;

        let row: Option<String> =
            sqlx::query_scalar("SELECT quarantined_at FROM models WHERE id = ?")
                .bind("m-noentry")
                .fetch_one(&state.db.pool)
                .await
                .expect("query");
        assert!(row.is_some());
    }

    // -----------------------------------------------------------------------
    // restart_after_crash — error paths (no Docker mock available, so we
    // exercise everything that fails before the docker.start_llamacpp call).
    // The pure rebuild_llamacpp_config helper covers the success-path
    // config-construction logic above; the docker orchestration around it
    // is small enough to read.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn restart_after_crash_missing_model_returns_err() {
        let state = build_test_state().await;
        let err = restart_after_crash(&state, "no-such-model")
            .await
            .unwrap_err();
        assert!(err.contains("missing persisted state"), "got: {err}");
    }

    #[tokio::test]
    async fn restart_after_crash_missing_secrets_returns_err() {
        let state = build_test_state().await;
        insert_model_row(&state, "m-no-secrets").await;
        // No container_secrets row inserted.
        let err = restart_after_crash(&state, "m-no-secrets")
            .await
            .unwrap_err();
        assert!(err.contains("container_secrets row missing"), "got: {err}");
    }

    #[tokio::test]
    async fn restart_after_crash_legacy_runtime_overrides_returns_err_without_quarantining() {
        let state = build_test_state().await;
        // Insert a model row with empty `{}` runtime_overrides (= legacy
        // shape, both sub-structs default).
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, context_length, runtime_overrides) \
             VALUES (?, ?, ?, 'llamacpp', 0, 8192, '{}')",
        )
        .bind("m-legacy")
        .bind("owner/repo")
        .bind("model.gguf")
        .execute(&state.db.pool)
        .await
        .expect("insert");

        sqlx::query(
            "INSERT INTO container_secrets (model_id, container_uid, api_key, parallel_slots) \
             VALUES (?, ?, ?, ?)",
        )
        .bind("m-legacy")
        .bind(11000_i64)
        .bind("api-key-legacy")
        .bind(1_i64)
        .execute(&state.db.pool)
        .await
        .expect("insert secrets");

        let err = restart_after_crash(&state, "m-legacy").await.unwrap_err();
        assert!(err.contains("legacy runtime_overrides"), "got: {err}");

        // restart_after_crash itself MUST NOT write the quarantine columns
        // (the supervisor wrapper does that on Err).
        let row: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT quarantined_at, quarantine_reason FROM models WHERE id = ?")
                .bind("m-legacy")
                .fetch_one(&state.db.pool)
                .await
                .expect("query");
        assert!(
            row.0.is_none(),
            "restart_after_crash must not set quarantined_at"
        );
        assert!(row.1.is_none());
    }
}
