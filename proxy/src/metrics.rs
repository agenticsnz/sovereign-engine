use std::collections::HashMap;

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::warn;

use crate::api::hf::{get_disk_usage, DiskUsage};
use crate::docker::DockerManager;
use crate::scheduler::gate::GateSnapshot;
use crate::scheduler::queue::QueueStats;

// ---- CPU sampling (Linux /proc/stat) ----

struct CpuTimes {
    idle: u64,
    total: u64,
}

pub struct CpuSampler {
    prev: Option<CpuTimes>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuInfo {
    pub utilization_percent: f64,
    pub num_cores: u32,
}

impl CpuSampler {
    fn new() -> Self {
        Self { prev: None }
    }

    /// Read /proc/stat, compute CPU % as delta vs previous reading.
    /// Returns None on first call (no delta) or on non-Linux.
    fn sample(&mut self) -> Option<CpuInfo> {
        let contents = std::fs::read_to_string("/proc/stat").ok()?;
        let mut num_cores: u32 = 0;
        let mut aggregate_line: Option<&str> = None;

        for line in contents.lines() {
            if line.starts_with("cpu ") {
                aggregate_line = Some(line);
            } else if line.starts_with("cpu")
                && line.as_bytes().get(3).is_some_and(|b| b.is_ascii_digit())
            {
                num_cores += 1;
            }
        }

        let line = aggregate_line?;
        // cpu  user nice system idle iowait irq softirq steal ...
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1) // skip "cpu"
            .take(8) // user nice system idle iowait irq softirq steal
            .filter_map(|s| s.parse().ok())
            .collect();

        if fields.len() < 4 {
            return None;
        }

        let idle = fields[3] + fields.get(4).copied().unwrap_or(0); // idle + iowait
        let total: u64 = fields.iter().sum();

        let current = CpuTimes { idle, total };

        let result = if let Some(prev) = &self.prev {
            let d_total = current.total.saturating_sub(prev.total);
            let d_idle = current.idle.saturating_sub(prev.idle);
            if d_total == 0 {
                None
            } else {
                let pct = ((d_total - d_idle) as f64 / d_total as f64) * 100.0;
                Some(CpuInfo {
                    utilization_percent: (pct * 10.0).round() / 10.0, // 1 decimal place
                    num_cores: num_cores.max(1),
                })
            }
        } else {
            None
        };

        self.prev = Some(current);
        result
    }
}

/// How often the collector runs (seconds).
const COLLECT_INTERVAL_SECS: u64 = 2;

/// Broadcast channel buffer size — enough for a couple of slow readers.
const BROADCAST_BUFFER: usize = 4;

/// A point-in-time snapshot of system metrics, sent to SSE clients.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub gpu_memory: Vec<GpuMemoryInfo>,
    pub cpu: Option<CpuInfo>,
    pub containers: Vec<ContainerStatus>,
    pub queues: HashMap<String, QueueStats>,
    pub gates: HashMap<String, GateSnapshot>,
    pub disk: Option<DiskInfo>,
    pub active_reservation: Option<ActiveReservationInfo>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveReservationInfo {
    pub reservation_id: String,
    pub user_id: String,
    pub user_display_name: Option<String>,
    pub end_time: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuMemoryInfo {
    pub gpu_type: String,
    pub device_index: u32,
    pub total_mb: u64,
    pub used_mb: u64,
    pub free_mb: u64,
    pub utilization_percent: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerStatus {
    pub model_id: String,
    pub backend_type: String,
    pub healthy: bool,
    pub state: Option<String>,
    pub vram_used_mb: Option<u64>,
    // Phase 7: supervisor + crash diagnostics surfaced to the admin UI.
    /// FSM state from the supervisor (None for non-llamacpp containers or
    /// containers without a supervisor_map entry yet). Serialised as one of
    /// `"Starting" | "Healthy" | "Suspect" | "Crashed" | "Quarantined"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fsm_state: Option<String>,
    /// Whether `models.quarantined_at IS NOT NULL`.
    #[serde(default)]
    pub quarantined: bool,
    /// Reason from `models.quarantine_reason`, if quarantined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
    /// Most-recent `backend_crash_log` row for this model, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_crash: Option<LastCrashSummary>,
}

/// Summary of the most-recent crash event for a backend.
///
/// Surfaced via `extract_container_statuses_enriched` so the admin UI can
/// flag a one-line "last crashed at …, exit 137, OOM" per model without an
/// extra round-trip. Full history is fetched on demand via
/// `GET /api/admin/models/:id/crash_history`.
#[derive(Debug, Clone, Serialize)]
pub struct LastCrashSummary {
    pub occurred_at: String,
    pub exit_code: Option<i64>,
    pub oom_killed: bool,
    /// True iff `backend_crash_log.log_path` is non-NULL — i.e. a tail was
    /// captured. The UI uses this to gate the "view log" link; a 404 is
    /// still possible if the file has since been GC'd.
    pub log_path_present: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

impl From<DiskUsage> for DiskInfo {
    fn from(d: DiskUsage) -> Self {
        Self {
            total_bytes: d.total_bytes,
            used_bytes: d.used_bytes,
            free_bytes: d.free_bytes,
        }
    }
}

/// Holds the broadcast sender; cloned into AppState.
#[derive(Debug, Clone)]
pub struct MetricsBroadcaster {
    tx: broadcast::Sender<MetricsSnapshot>,
}

impl MetricsBroadcaster {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_BUFFER);
        Self { tx }
    }

    /// Subscribe to the metrics stream (one receiver per SSE client).
    pub fn subscribe(&self) -> broadcast::Receiver<MetricsSnapshot> {
        self.tx.subscribe()
    }

    /// Spawn the background collector task. Call once after AppState is built.
    ///
    /// Phase 7: takes the full `AppState` Arc so the per-container status
    /// merge can call [`crate::api::common::extract_container_statuses_enriched`]
    /// — which surfaces supervisor FSM state, quarantine flags, and the
    /// most-recent crash summary into the SSE feed the admin UI consumes.
    pub fn spawn_collector(&self, state: std::sync::Arc<crate::AppState>) {
        let tx = self.tx.clone();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(COLLECT_INTERVAL_SECS));
            let mut cpu_sampler = CpuSampler::new();

            loop {
                interval.tick().await;

                let snapshot = collect_snapshot(&state, &mut cpu_sampler).await;

                // If nobody is listening, send() returns Err — that's fine.
                let _ = tx.send(snapshot);
            }
        });
    }
}

async fn collect_snapshot(
    state: &std::sync::Arc<crate::AppState>,
    cpu_sampler: &mut CpuSampler,
) -> MetricsSnapshot {
    let docker = &state.docker;
    let scheduler = &state.scheduler;
    let model_path = state.config.model_path.as_str();

    // GPU stats (memory + utilization) — all detected GPUs
    let gpu_memory: Vec<GpuMemoryInfo> = DockerManager::gpu_all_info()
        .await
        .into_iter()
        .map(|stats| GpuMemoryInfo {
            gpu_type: stats.gpu_type,
            device_index: stats.device_index,
            total_mb: stats.total_mb,
            used_mb: stats.used_mb,
            free_mb: stats.free_mb,
            utilization_percent: stats.utilization_percent,
        })
        .collect();

    // CPU utilization (delta-based)
    let cpu = cpu_sampler.sample();

    // Per-container VRAM (best-effort, requires pid:host)
    let vram_map = docker.per_container_vram().await;

    // Container statuses (Phase 7 enriched: supervisor FSM + quarantine +
    // last_crash). The SSE snapshot is what the admin UI reads live, so the
    // enriched variant must run here too — not just on the REST endpoint.
    let containers = match docker.list_managed_containers().await {
        Ok(list) => {
            crate::api::common::extract_container_statuses_enriched(state, list, &vram_map).await
        }
        Err(e) => {
            warn!(error = %e, "Failed to list containers for metrics");
            vec![]
        }
    };

    // Queue stats + gate status
    let queues = scheduler.get_queue_stats().await;
    let gates = scheduler.gate().status().await;

    // Disk usage (blocking syscall, but fast enough for a 2s interval)
    let disk = get_disk_usage(model_path).ok().map(DiskInfo::from);

    // Active reservation
    let active_reservation = scheduler
        .active_reservation()
        .await
        .map(|a| ActiveReservationInfo {
            reservation_id: a.reservation_id,
            user_id: a.user_id,
            user_display_name: a.user_display_name,
            end_time: a.end_time,
        });

    let timestamp = chrono::Utc::now().to_rfc3339();

    MetricsSnapshot {
        gpu_memory,
        cpu,
        containers,
        queues,
        gates,
        disk,
        active_reservation,
        timestamp,
    }
}
