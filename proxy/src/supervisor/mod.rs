//! Backend container supervisor.
//!
//! Per-`AppState` map of `model_id -> BackendState`. A 10s tick task probes
//! every loaded backend's `/health` endpoint, classifies the response per
//! the design table (note `019dd7f3-5917-72a2-99b0-e4dd52166f1c` on parent
//! card `019db7fc-4464-7730-b3c4-33caa90cb928`), transitions the FSM, and
//! reconciles state via [`crate::api::common::reconcile_dead_backend`] when
//! `Crashed` is reached.
//!
//! The supervisor also receives "probe-now" kicks via an mpsc channel from
//! the proxy hot path (`proxy::streaming`). Any non-2xx outcome — both
//! connect-failure and 5xx response — fires a kick so the supervisor can
//! diagnose immediately rather than wait for the next tick.
//!
//! ## Phase 3 scope
//!
//! Phase 3 only detects, classifies, and transitions to `Crashed`. It does
//! **not** auto-restart, capture exit_code/OOMKilled/log tail, or interact
//! with the `worked` flag. `Crashed` is terminal in this phase — operators
//! restart manually via the existing UI. Phase 5 adds diagnostics capture;
//! Phase 6 adds restart and quarantine. The `Quarantined` variant of
//! [`BackendFsmState`] exists so consumers can pattern-match exhaustively
//! once Phase 6 lands, but is never produced by Phase 3.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub mod capture;
// Re-export the Phase 5 capture types/functions at the supervisor module
// root so callers can write `crate::supervisor::CrashCapture` etc. without
// reaching into the submodule. The functions are also used directly via
// `capture::…` from `handle_kick` below.
#[allow(unused_imports)]
pub use capture::{
    capture_crash_state, gc_crash_logs, write_crash_log_file, CrashCapture, CRASH_LOGS_MAX_BYTES,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Lifecycle state for a single backend container.
///
/// See the FSM diagram in design note `019dd7f3-5917-72a2-99b0-e4dd52166f1c`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFsmState {
    /// Container started; `/health` may still return 503 while loading.
    Starting,
    /// `/health` returned 200 at least once; no consecutive failures.
    Healthy,
    /// One non-success outcome since last `Healthy`. One more flips to `Crashed`.
    Suspect,
    /// Detected dead/unresponsive. Terminal in Phase 3; Phase 6 adds the
    /// restart/quarantine branch.
    Crashed,
    /// Set by Phase 6 when a backend that never served a successful response
    /// crashes. Phase 3 never enters this state — listed for exhaustive
    /// pattern-matching only.
    Quarantined,
}

/// Per-backend state held in the supervisor map.
#[derive(Debug, Clone)]
pub struct BackendState {
    /// Current lifecycle position.
    pub fsm: BackendFsmState,
    /// Count of consecutive non-success outcomes since last `Healthy`.
    /// Reset to 0 on every `HealthyOk`.
    pub consecutive_failures: u8,
    /// When the supervisor first observed this backend (or its current start).
    /// Used to enforce the 5-minute startup grace.
    pub started_at: Instant,
}

/// Per-`AppState` map of model_id → BackendState.
pub type SupervisorMap = Arc<DashMap<String, BackendState>>;

/// Why a probe is being issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeReason {
    /// Periodic 10s tick — probe every loaded model.
    Tick,
    /// Hot path saw a non-2xx — probe this model immediately.
    OnFailure,
}

/// Message sent from the hot path (or the tick task) into the supervisor.
#[derive(Debug, Clone)]
pub struct ProbeKick {
    pub model_id: String,
    pub reason: ProbeReason,
}

/// Outcome of a single probe. Pure data — no I/O — so the FSM transition
/// function can be unit-tested without a network or Docker stub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// `/health` returned 200.
    HealthyOk,
    /// `/health` returned 503 with body indicating model is loading.
    /// (We don't bother parsing the body — status code alone is enough per
    /// the verified design table.)
    LoadingFiveOhThree,
    /// Connect refused, timeout, DNS failure, or other transport error.
    TransportFailure,
    /// `/health` returned an unexpected status (e.g. 500, 502, 504).
    OtherFailure,
    /// Docker reports the container is stopped (or gone).
    ContainerStopped,
}

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Interval between periodic probe ticks.
pub const TICK_INTERVAL: Duration = Duration::from_secs(10);

/// HTTP timeout for a single `/health` probe.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// While a backend is `Starting`, transport failures and 503s are tolerated
/// for this long before they can flip the state to `Crashed`.
pub const STARTUP_GRACE: Duration = Duration::from_secs(5 * 60);

/// Number of consecutive non-success outcomes that triggers `Crashed`.
pub const FAILURE_THRESHOLD: u8 = 2;

// ---------------------------------------------------------------------------
// Pure FSM transition
// ---------------------------------------------------------------------------

/// Pure FSM transition function — **no I/O**. Easy to unit-test.
///
/// Returns the new [`BackendState`]. The caller is responsible for any side
/// effects (e.g. calling [`crate::api::common::reconcile_dead_backend`] when
/// the new state is `Crashed`).
///
/// See the design note's classification table for the truth source. A
/// summary:
///
/// | Outcome              | While `Starting`                        | While `Healthy`/`Suspect`            |
/// |----------------------|-----------------------------------------|--------------------------------------|
/// | `HealthyOk`          | → `Healthy`, reset failures             | → `Healthy`, reset failures          |
/// | `LoadingFiveOhThree` | stay `Starting`, no fail count          | count toward failure                 |
/// | `TransportFailure`   | count toward failure (5-min grace)      | count toward failure                 |
/// | `OtherFailure`       | count toward failure                    | count toward failure                 |
/// | `ContainerStopped`   | immediate `Crashed`                     | immediate `Crashed`                  |
///
/// `Crashed` and `Quarantined` are terminal in Phase 3 — the function
/// returns the state unchanged.
pub fn transition(prev: &BackendState, outcome: ProbeOutcome) -> BackendState {
    // Crashed/Quarantined are terminal in Phase 3.
    if matches!(
        prev.fsm,
        BackendFsmState::Crashed | BackendFsmState::Quarantined
    ) {
        return prev.clone();
    }

    // Container vanishing always crashes immediately, regardless of the
    // failure counter or startup grace.
    if outcome == ProbeOutcome::ContainerStopped {
        return BackendState {
            fsm: BackendFsmState::Crashed,
            consecutive_failures: prev.consecutive_failures.saturating_add(1),
            started_at: prev.started_at,
        };
    }

    // 200 OK always returns to Healthy and resets the failure counter.
    if outcome == ProbeOutcome::HealthyOk {
        return BackendState {
            fsm: BackendFsmState::Healthy,
            consecutive_failures: 0,
            started_at: prev.started_at,
        };
    }

    // Startup grace — while in Starting, 503-loading and transport failures
    // are tolerated for up to STARTUP_GRACE. After that, the next non-2xx
    // outcome counts toward the failure threshold like any other state.
    let in_grace = prev.fsm == BackendFsmState::Starting
        && Instant::now().saturating_duration_since(prev.started_at) <= STARTUP_GRACE;

    // 503-loading during grace: stay Starting, don't increment failures.
    if outcome == ProbeOutcome::LoadingFiveOhThree && in_grace {
        return BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: 0,
            started_at: prev.started_at,
        };
    }

    // Transport failure during grace: tolerate (don't crash even if it
    // would push us over the threshold) but increment the counter so a
    // burst right at grace-end behaves predictably.
    if outcome == ProbeOutcome::TransportFailure && in_grace {
        return BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: prev.consecutive_failures.saturating_add(1),
            started_at: prev.started_at,
        };
    }

    // Default failure path: increment the counter; flip to Crashed once
    // we hit the threshold, otherwise mark Suspect (Healthy → Suspect on
    // first failure, Starting → Suspect after grace expiry, etc.).
    let new_failures = prev.consecutive_failures.saturating_add(1);
    let new_state = if new_failures >= FAILURE_THRESHOLD {
        BackendFsmState::Crashed
    } else {
        BackendFsmState::Suspect
    };

    BackendState {
        fsm: new_state,
        consecutive_failures: new_failures,
        started_at: prev.started_at,
    }
}

// ---------------------------------------------------------------------------
// Worked-flag read helper (Phase 4)
// ---------------------------------------------------------------------------

/// Read the "has this backend served a 2xx since current start?" flag.
///
/// Memory-first: the in-memory atomic is the source of truth while a
/// container is alive. Falls back to the `models.worked` DB column on a
/// cold-start cache miss (i.e. the proxy restarted but the container kept
/// running and the DashMap entry was lost).
///
/// Phase 4 only exposes the helper — Phase 6 wires it into the supervisor's
/// `Crashed` branch to drive the quarantine decision.
#[allow(dead_code)]
pub async fn read_worked(state: &Arc<crate::AppState>, model_id: &str) -> bool {
    if let Some(entry) = state.worked_map.get(model_id) {
        return entry.load(std::sync::atomic::Ordering::SeqCst);
    }
    // Cold-start fallback: in-memory entry was lost (proxy restarted).
    let row: Option<(i64,)> = sqlx::query_as("SELECT worked FROM models WHERE id = ?")
        .bind(model_id)
        .fetch_optional(&state.db.pool)
        .await
        .ok()
        .flatten();
    row.map(|(w,)| w != 0).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Probe classification
// ---------------------------------------------------------------------------

/// Issue a single probe and classify the response.
///
/// Performs a `GET <url>` with [`PROBE_TIMEOUT`]; classifies status codes
/// per the design table.
pub async fn classify_probe(client: &reqwest::Client, url: &str) -> ProbeOutcome {
    let result = client.get(url).timeout(PROBE_TIMEOUT).send().await;
    match result {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                ProbeOutcome::HealthyOk
            } else if status.as_u16() == 503 {
                ProbeOutcome::LoadingFiveOhThree
            } else {
                ProbeOutcome::OtherFailure
            }
        }
        Err(_) => ProbeOutcome::TransportFailure,
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Build the kick channel pair without starting the tick task.
///
/// Use this when [`crate::AppState`] needs the `tx` end at construction time
/// — call [`spawn`] later with the matching `rx` and the built `Arc<AppState>`
/// to start the supervisor loop.
pub fn channel() -> (mpsc::Sender<ProbeKick>, mpsc::Receiver<ProbeKick>) {
    mpsc::channel::<ProbeKick>(64)
}

/// Start the supervisor tick task and the kick-channel drain loop.
///
/// `seed_model_ids` are models that `recover_gate_state` confirmed alive at
/// proxy startup — they're seeded as `Healthy` with `consecutive_failures = 0`
/// and `started_at = now`. Newly-loaded models register themselves into the
/// map via [`register_starting`] when their container is launched.
///
/// The caller supplies the `(tx, rx)` pair from [`channel`] so `tx` can be
/// stored on `AppState` before this function is called.
pub fn spawn(
    state: Arc<crate::AppState>,
    rx: mpsc::Receiver<ProbeKick>,
    seed_model_ids: Vec<String>,
) {
    // Initialise the map with seed entries (Healthy).
    let now = Instant::now();
    for id in &seed_model_ids {
        state.supervisor_map.insert(
            id.clone(),
            BackendState {
                fsm: BackendFsmState::Healthy,
                consecutive_failures: 0,
                started_at: now,
            },
        );
    }
    info!(seeded = seed_model_ids.len(), "Supervisor seeded");

    let tick_tx = state.probe_tx.clone();
    tokio::spawn(supervisor_loop(state, rx, tick_tx));
}

/// Register a fresh `Starting` entry — used by Phase 6 (and tests) when a
/// new container is launched. Phase 3 doesn't call this from the launch
/// path; the seed list at startup plus the kick channel cover all detection
/// paths for now.
#[allow(dead_code)]
pub fn register_starting(map: &SupervisorMap, model_id: &str) {
    map.insert(
        model_id.to_string(),
        BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: 0,
            started_at: Instant::now(),
        },
    );
}

/// Main supervisor loop — owns the receive end of the kick channel and the
/// tick interval. Splits incoming work into per-model probes.
async fn supervisor_loop(
    state: Arc<crate::AppState>,
    mut rx: mpsc::Receiver<ProbeKick>,
    tick_tx: mpsc::Sender<ProbeKick>,
) {
    // Spawn a separate tick generator that synthesises Tick kicks for every
    // entry in the map. We reuse the same channel so the loop body has a
    // single source of work.
    let tick_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        // Skip the immediate-fire first tick.
        interval.tick().await;
        loop {
            interval.tick().await;
            // Snapshot keys so we don't hold the dashmap shard while sending.
            let ids: Vec<String> = tick_state
                .supervisor_map
                .iter()
                .map(|e| e.key().clone())
                .collect();
            for id in ids {
                let _ = tick_tx
                    .try_send(ProbeKick {
                        model_id: id,
                        reason: ProbeReason::Tick,
                    })
                    .map_err(|e| {
                        debug!(error = ?e, "Supervisor tick: kick channel full or closed");
                    });
            }
        }
    });

    let client = reqwest::Client::new();

    while let Some(kick) = rx.recv().await {
        handle_kick(&state, &client, kick).await;
    }
}

/// Process a single kick: probe the model and apply the FSM transition.
async fn handle_kick(state: &Arc<crate::AppState>, client: &reqwest::Client, kick: ProbeKick) {
    // Look up the prior state. If the model isn't tracked yet we add it as
    // Starting on first observation — covers the "OnFailure kick before
    // tick has seen this model" race.
    let prev = state
        .supervisor_map
        .get(&kick.model_id)
        .map(|e| e.clone())
        .unwrap_or_else(|| BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: 0,
            started_at: Instant::now(),
        });

    if matches!(
        prev.fsm,
        BackendFsmState::Crashed | BackendFsmState::Quarantined
    ) {
        // Terminal — don't bother probing.
        return;
    }

    // Look up backend type to build the URL. Best-effort: if the model row
    // is gone, skip — reconcile will already have been called.
    let backend_type =
        crate::api::common::lookup_backend_type(&state.db.pool, &kick.model_id).await;
    let base = state
        .docker
        .backend_base_url(&kick.model_id, &backend_type);
    let health_url = format!("{}/health", base.trim_end_matches('/'));

    // Probe transport / HTTP status.
    let mut outcome = classify_probe(client, &health_url).await;

    // On any non-success, also inspect the container so we can promote
    // `TransportFailure` to `ContainerStopped` when the container is
    // actually gone. This matches the design table's "Docker reports
    // container stopped → immediate Crashed" row.
    if outcome != ProbeOutcome::HealthyOk {
        let container_name = format!("sovereign-llamacpp-{}", kick.model_id);
        match state
            .docker
            .docker
            .inspect_container(&container_name, None)
            .await
        {
            Ok(info) => {
                let running = info
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);
                if !running {
                    outcome = ProbeOutcome::ContainerStopped;
                }
            }
            Err(_) => {
                // Inspect failed — could be Docker unreachable, container
                // truly gone, or a transient error. Treat as stopped only
                // if we already have a transport failure; otherwise leave
                // the original classification.
                if outcome == ProbeOutcome::TransportFailure {
                    outcome = ProbeOutcome::ContainerStopped;
                }
            }
        }
    }

    let next = transition(&prev, outcome);

    debug!(
        model = %kick.model_id,
        ?prev.fsm,
        ?outcome,
        ?next.fsm,
        reason = ?kick.reason,
        "Supervisor probe",
    );

    let crashed_now = next.fsm == BackendFsmState::Crashed && prev.fsm != BackendFsmState::Crashed;

    state
        .supervisor_map
        .insert(kick.model_id.clone(), next.clone());

    if crashed_now {
        warn!(
            model = %kick.model_id,
            ?prev.fsm,
            ?outcome,
            "Supervisor flipped backend to Crashed",
        );

        // SAFETY (Phase 5): capture MUST run before any code path that could
        // remove the container. `start_llamacpp` (proxy/src/docker/llamacpp.rs:155)
        // calls `remove_container` on any non-running container before
        // starting fresh — once Docker removes the container, its logs are
        // gone forever. Phase 6 will add the restart call **after** this
        // capture/reconcile block; do not reorder.
        let container_name = format!("sovereign-llamacpp-{}", kick.model_id);
        let crash_capture = capture::capture_crash_state(
            &state.docker.docker,
            &container_name,
        )
        .await;
        let unix_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let data_path = std::path::Path::new(&state.config.data_path);
        let log_path = capture::write_crash_log_file(
            data_path,
            &kick.model_id,
            unix_ts,
            &crash_capture.log_tail,
        )
        .await;
        crate::api::common::reconcile_dead_backend_with_capture(
            state,
            &kick.model_id,
            &crash_capture,
            log_path.as_deref(),
            "supervisor_probe_failure",
        )
        .await;
        let crash_logs_dir = data_path.join("crash_logs");
        capture::gc_crash_logs(&crash_logs_dir, capture::CRASH_LOGS_MAX_BYTES).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn starting_state() -> BackendState {
        BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: 0,
            started_at: Instant::now(),
        }
    }

    fn healthy_state() -> BackendState {
        BackendState {
            fsm: BackendFsmState::Healthy,
            consecutive_failures: 0,
            started_at: Instant::now(),
        }
    }

    fn suspect_state(failures: u8) -> BackendState {
        BackendState {
            fsm: BackendFsmState::Suspect,
            consecutive_failures: failures,
            started_at: Instant::now(),
        }
    }

    // -----------------------------------------------------------------------
    // Pure-FSM transition tests — one per row of the classification table.
    // -----------------------------------------------------------------------

    #[test]
    fn transition_starting_on_healthy_ok_goes_healthy_resets_failures() {
        let mut prev = starting_state();
        prev.consecutive_failures = 1;
        let next = transition(&prev, ProbeOutcome::HealthyOk);
        assert_eq!(next.fsm, BackendFsmState::Healthy);
        assert_eq!(next.consecutive_failures, 0);
    }

    #[test]
    fn transition_starting_on_loading_503_stays_starting() {
        let prev = starting_state();
        let next = transition(&prev, ProbeOutcome::LoadingFiveOhThree);
        assert_eq!(next.fsm, BackendFsmState::Starting);
        assert_eq!(next.consecutive_failures, 0);
    }

    #[test]
    fn transition_starting_on_transport_fail_within_grace_increments_failures_no_crash() {
        let prev = starting_state(); // started_at = now, fully within grace.
        let next = transition(&prev, ProbeOutcome::TransportFailure);
        assert_eq!(next.fsm, BackendFsmState::Starting);
        assert_eq!(next.consecutive_failures, 1);

        // A second transport failure inside grace should still NOT crash.
        let next2 = transition(&next, ProbeOutcome::TransportFailure);
        assert_eq!(
            next2.fsm,
            BackendFsmState::Starting,
            "still in startup grace, must not crash"
        );
        assert_eq!(next2.consecutive_failures, 2);
    }

    #[test]
    fn transition_starting_on_transport_fail_after_grace_expiry_crashes_when_threshold_hit() {
        // Simulate a model that started 6 minutes ago — past the 5-minute grace.
        let prev = BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: 0,
            started_at: Instant::now() - Duration::from_secs(6 * 60),
        };
        let next = transition(&prev, ProbeOutcome::TransportFailure);
        assert_eq!(next.fsm, BackendFsmState::Suspect);
        assert_eq!(next.consecutive_failures, 1);

        let next2 = transition(&next, ProbeOutcome::TransportFailure);
        assert_eq!(next2.fsm, BackendFsmState::Crashed);
        assert_eq!(next2.consecutive_failures, 2);
    }

    #[test]
    fn transition_healthy_on_healthy_ok_stays_healthy_resets_failures() {
        let prev = healthy_state();
        let next = transition(&prev, ProbeOutcome::HealthyOk);
        assert_eq!(next.fsm, BackendFsmState::Healthy);
        assert_eq!(next.consecutive_failures, 0);
    }

    #[test]
    fn transition_healthy_on_loading_503_increments_failures() {
        let prev = healthy_state();
        let next = transition(&prev, ProbeOutcome::LoadingFiveOhThree);
        assert_eq!(next.fsm, BackendFsmState::Suspect);
        assert_eq!(next.consecutive_failures, 1);
    }

    #[test]
    fn transition_healthy_on_transport_fail_increments_failures() {
        let prev = healthy_state();
        let next = transition(&prev, ProbeOutcome::TransportFailure);
        assert_eq!(next.fsm, BackendFsmState::Suspect);
        assert_eq!(next.consecutive_failures, 1);
    }

    #[test]
    fn transition_healthy_on_other_failure_increments_failures() {
        let prev = healthy_state();
        let next = transition(&prev, ProbeOutcome::OtherFailure);
        assert_eq!(next.fsm, BackendFsmState::Suspect);
        assert_eq!(next.consecutive_failures, 1);
    }

    #[test]
    fn transition_suspect_on_healthy_ok_returns_to_healthy() {
        let prev = suspect_state(1);
        let next = transition(&prev, ProbeOutcome::HealthyOk);
        assert_eq!(next.fsm, BackendFsmState::Healthy);
        assert_eq!(next.consecutive_failures, 0);
    }

    #[test]
    fn transition_suspect_on_second_failure_crashes() {
        let prev = suspect_state(1);
        let next = transition(&prev, ProbeOutcome::TransportFailure);
        assert_eq!(next.fsm, BackendFsmState::Crashed);
        assert_eq!(next.consecutive_failures, 2);
    }

    #[test]
    fn transition_any_on_container_stopped_crashes_immediately() {
        for prev in [
            starting_state(),
            healthy_state(),
            suspect_state(1),
        ] {
            let next = transition(&prev, ProbeOutcome::ContainerStopped);
            assert_eq!(
                next.fsm,
                BackendFsmState::Crashed,
                "ContainerStopped must crash immediately from {:?}",
                prev.fsm,
            );
        }
    }

    #[test]
    fn transition_crashed_is_terminal() {
        let prev = BackendState {
            fsm: BackendFsmState::Crashed,
            consecutive_failures: 2,
            started_at: Instant::now(),
        };
        // No outcome should move out of Crashed in Phase 3.
        for outcome in [
            ProbeOutcome::HealthyOk,
            ProbeOutcome::LoadingFiveOhThree,
            ProbeOutcome::TransportFailure,
            ProbeOutcome::OtherFailure,
            ProbeOutcome::ContainerStopped,
        ] {
            let next = transition(&prev, outcome);
            assert_eq!(
                next.fsm,
                BackendFsmState::Crashed,
                "Crashed must be terminal, but outcome {:?} moved out",
                outcome,
            );
        }
    }

    #[test]
    fn transition_quarantined_is_terminal_in_phase_3() {
        // Phase 3 never produces Quarantined, but the FSM must not move out
        // of it if Phase 6 (or a test) seeds it.
        let prev = BackendState {
            fsm: BackendFsmState::Quarantined,
            consecutive_failures: 0,
            started_at: Instant::now(),
        };
        let next = transition(&prev, ProbeOutcome::HealthyOk);
        assert_eq!(next.fsm, BackendFsmState::Quarantined);
    }

    // -----------------------------------------------------------------------
    // 5-minute grace expiry — explicit boundary check.
    // -----------------------------------------------------------------------

    #[test]
    fn grace_expiry_six_minutes_with_two_transport_failures_crashes() {
        let prev = BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: 0,
            started_at: Instant::now() - Duration::from_secs(6 * 60),
        };
        let next = transition(&prev, ProbeOutcome::TransportFailure);
        let next2 = transition(&next, ProbeOutcome::TransportFailure);
        assert_eq!(next2.fsm, BackendFsmState::Crashed);
    }

    #[test]
    fn grace_active_at_thirty_seconds_does_not_crash() {
        let prev = BackendState {
            fsm: BackendFsmState::Starting,
            consecutive_failures: 0,
            started_at: Instant::now() - Duration::from_secs(30),
        };
        let next = transition(&prev, ProbeOutcome::TransportFailure);
        let next2 = transition(&next, ProbeOutcome::TransportFailure);
        assert_eq!(next2.fsm, BackendFsmState::Starting);
        assert_eq!(next2.consecutive_failures, 2);
    }

    // -----------------------------------------------------------------------
    // Seed test — spawn() seeds the supervisor map.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_seeds_supervisor_map_with_healthy_entries() {
        // Build a minimal AppState — we only exercise the seeding side
        // effect, not the tick task.
        let state = build_test_state().await;
        let (_, rx) = channel();
        spawn(state.clone(), rx, vec!["foo".to_string(), "bar".to_string()]);

        let foo = state.supervisor_map.get("foo").expect("foo seeded");
        assert_eq!(foo.fsm, BackendFsmState::Healthy);
        assert_eq!(foo.consecutive_failures, 0);

        let bar = state.supervisor_map.get("bar").expect("bar seeded");
        assert_eq!(bar.fsm, BackendFsmState::Healthy);
    }

    #[tokio::test]
    async fn spawn_with_empty_seeds_produces_empty_map() {
        let state = build_test_state().await;
        let (_, rx) = channel();
        spawn(state.clone(), rx, vec![]);
        assert!(state.supervisor_map.is_empty());
    }

    #[tokio::test]
    async fn register_starting_inserts_starting_entry() {
        let map: SupervisorMap = Arc::new(DashMap::new());
        register_starting(&map, "alpha");
        let s = map.get("alpha").expect("alpha registered");
        assert_eq!(s.fsm, BackendFsmState::Starting);
        assert_eq!(s.consecutive_failures, 0);
    }

    // -----------------------------------------------------------------------
    // Channel contract — the kick channel can be sent into.
    // (Full end-to-end probe flow needs a Docker stub; out of scope here.
    //  We assert the cheap property: try_send into the returned tx works,
    //  and the receiver loop drains it without deadlocking.)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn kick_channel_accepts_try_send() {
        // Bounded(64) fresh channel — verify the contract that hot-path
        // `try_send` doesn't block. We don't run the supervisor loop here;
        // the loop side has its own integration coverage via Docker.
        let (tx, mut rx) = channel();
        let result = tx.try_send(ProbeKick {
            model_id: "no-such-model".to_string(),
            reason: ProbeReason::OnFailure,
        });
        assert!(result.is_ok(), "try_send into fresh channel should succeed");

        let kick = rx.recv().await.expect("kick should arrive");
        assert_eq!(kick.model_id, "no-such-model");
        assert_eq!(kick.reason, ProbeReason::OnFailure);
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    async fn build_test_state() -> Arc<crate::AppState> {
        use crate::config::AppConfig;
        use crate::db::Database;
        use crate::docker::DockerManager;
        use crate::metrics::MetricsBroadcaster;
        use crate::scheduler::reservation::ReservationBroadcaster;
        use crate::scheduler::Scheduler;

        let db = Database::test_db().await;
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
                model_path: "/tmp/test-models-supervisor".to_string(),
                model_host_path: "/tmp/test-models-supervisor".to_string(),
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
            supervisor_map: Arc::new(DashMap::new()),
            probe_tx: {
                // Test state needs a probe_tx field; produce a dummy
                // sender whose receiver is dropped. Hot-path code uses
                // try_send and tolerates "channel closed" gracefully.
                let (tx, _rx) = mpsc::channel::<ProbeKick>(1);
                tx
            },
            worked_map: Arc::new(DashMap::new()),
        })
    }
}
