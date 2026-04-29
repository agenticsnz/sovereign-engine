//! Crash diagnostics capture (Phase 5).
//!
//! Order of operations after the supervisor decides a backend has Crashed:
//!
//!   1. [`capture_crash_state`] — bollard `inspect_container` + `logs(tail=500)`.
//!      MUST run **before** any code path that could remove the container.
//!      Once Docker removes the container its logs are gone forever
//!      (`proxy/src/docker/llamacpp.rs:155` removes any stopped container at
//!      next start).
//!   2. [`write_crash_log_file`] — persist the captured log tail to
//!      `<data_path>/crash_logs/<model_id>-<unix_ts>.log` (mkdir -p).
//!   3. [`crate::api::common::reconcile_dead_backend_with_capture`] —
//!      INSERT enriched `backend_crash_log` row, clear loaded, drop gate +
//!      worked entry.
//!   4. [`gc_crash_logs`] — single inline pass; while dir > 1 GiB, delete
//!      oldest by mtime.
//!
//! See design note `019dd7f3-5917-72a2-99b0-e4dd52166f1c` (Diagnostics
//! capture section) for the locked design.

use std::path::{Path, PathBuf};

use bollard::query_parameters::LogsOptionsBuilder;
use futures::StreamExt;
use tracing::{debug, warn};

/// Diagnostic data captured from a crashed container before removal.
#[derive(Debug, Clone, Default)]
pub struct CrashCapture {
    pub container_id: Option<String>,
    pub exit_code: Option<i64>,
    pub oom_killed: bool,
    pub finished_at: Option<String>,
    /// In Docker's `ContainerState` there is no dedicated "signal" field —
    /// the closest is `State.Error`, populated when the runtime reports a
    /// fatal signal or other error string. We surface it here so callers
    /// can persist a more descriptive value than the bare `discovery_reason`.
    pub signal: Option<String>,
    /// Raw log tail (UTF-8-lossy decoded). May be empty if logs() failed.
    pub log_tail: Vec<u8>,
}

/// Cap on lines fetched from `docker logs`. Matches the design note.
const LOG_TAIL_LINES: &str = "500";

/// Cap on the on-disk crash-log directory. Once exceeded, [`gc_crash_logs`]
/// evicts files oldest-first by mtime until the total drops below the cap.
pub const CRASH_LOGS_MAX_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Capture crash diagnostics from the named container before any removal.
///
/// Returns `Default::default()` (with empty fields) on any bollard error —
/// best-effort capture, never blocks reconciliation. All errors are logged
/// at `warn` level.
pub async fn capture_crash_state(
    docker: &bollard::Docker,
    container_name: &str,
) -> CrashCapture {
    let mut out = CrashCapture::default();

    // 1. Inspect — exit_code / oom_killed / finished_at / signal / id.
    match docker.inspect_container(container_name, None).await {
        Ok(detail) => {
            out.container_id = detail.id.clone();
            if let Some(state) = detail.state {
                out.exit_code = state.exit_code;
                out.oom_killed = state.oom_killed.unwrap_or(false);
                out.finished_at = state.finished_at;
                // bollard's ContainerState exposes the runtime's free-form
                // error string via `state.error`; that's where signal info
                // (e.g. "OCI runtime ... signal: killed") shows up. Surface
                // any non-empty value as `signal`.
                out.signal = state.error.filter(|s| !s.is_empty());
            }
        }
        Err(e) => {
            warn!(
                container = %container_name,
                error = %e,
                "capture_crash_state: inspect_container failed",
            );
        }
    }

    // 2. Logs tail — stdout + stderr, last LOG_TAIL_LINES lines.
    let opts = LogsOptionsBuilder::new()
        .stdout(true)
        .stderr(true)
        .tail(LOG_TAIL_LINES)
        .build();
    let mut stream = docker.logs(container_name, Some(opts));
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(output) => {
                let bytes = output.into_bytes();
                out.log_tail.extend_from_slice(&bytes);
            }
            Err(e) => {
                warn!(
                    container = %container_name,
                    error = %e,
                    "capture_crash_state: logs stream chunk failed",
                );
                // Continue draining — partial logs are better than nothing.
            }
        }
    }

    debug!(
        container = %container_name,
        exit_code = ?out.exit_code,
        oom_killed = out.oom_killed,
        log_bytes = out.log_tail.len(),
        "capture_crash_state complete",
    );

    out
}

/// Write the captured log tail to `<base_dir>/crash_logs/<model_id>-<unix_ts>.log`.
///
/// Returns the path written, or `None` if any I/O step fails (logged at
/// `warn` level, not propagated). Creates the parent dir with mkdir -p
/// semantics on first call.
pub async fn write_crash_log_file(
    base_dir: &Path,
    model_id: &str,
    unix_ts: i64,
    log_tail: &[u8],
) -> Option<PathBuf> {
    let dir = base_dir.join("crash_logs");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        warn!(
            error = %e,
            dir = %dir.display(),
            "write_crash_log_file: failed to create crash log dir",
        );
        return None;
    }
    let path = dir.join(format!("{model_id}-{unix_ts}.log"));
    if let Err(e) = tokio::fs::write(&path, log_tail).await {
        warn!(
            error = %e,
            path = %path.display(),
            "write_crash_log_file: failed to write crash log file",
        );
        return None;
    }
    Some(path)
}

/// Inline single-pass GC: while total dir size > `max_bytes`, delete oldest
/// `.log` file by mtime.
///
/// Best-effort: I/O errors are logged at `warn` level and do not propagate.
/// Non-`.log` files in the directory are ignored (we don't manage them).
pub async fn gc_crash_logs(dir: &Path, max_bytes: u64) {
    // 1. Read entries; collect (path, size, mtime) for each .log file.
    let mut read_dir = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) => {
            // NotFound is fine — nothing to GC.
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!(
                    dir = %dir.display(),
                    error = %e,
                    "gc_crash_logs: read_dir failed",
                );
            }
            return;
        }
    };

    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    loop {
        match read_dir.next_entry().await {
            Ok(Some(e)) => {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) != Some("log") {
                    continue;
                }
                let meta = match e.metadata().await {
                    Ok(m) => m,
                    Err(err) => {
                        debug!(
                            path = %path.display(),
                            error = %err,
                            "gc_crash_logs: skip entry — metadata failed",
                        );
                        continue;
                    }
                };
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                let size = meta.len();
                total = total.saturating_add(size);
                entries.push((path, size, mtime));
            }
            Ok(None) => break,
            Err(e) => {
                warn!(
                    dir = %dir.display(),
                    error = %e,
                    "gc_crash_logs: next_entry failed",
                );
                break;
            }
        }
    }

    if total <= max_bytes {
        return;
    }

    // 2. Sort ascending by mtime — oldest first.
    entries.sort_by_key(|(_, _, mtime)| *mtime);

    // 3. Delete oldest until under budget.
    let mut evicted = 0usize;
    for (path, size, _) in entries {
        if total <= max_bytes {
            break;
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                total = total.saturating_sub(size);
                evicted += 1;
                debug!(
                    path = %path.display(),
                    size,
                    "gc_crash_logs: evicted",
                );
            }
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "gc_crash_logs: remove_file failed",
                );
            }
        }
    }

    if evicted > 0 {
        debug!(
            dir = %dir.display(),
            evicted,
            remaining_bytes = total,
            "gc_crash_logs done",
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_crash_log_file_creates_dir_and_file() {
        let tmp = TempDir::new().expect("tempdir");
        let model_id = "model-1";
        let ts = 1_700_000_000;
        let payload = b"hello crash";

        let path = write_crash_log_file(tmp.path(), model_id, ts, payload)
            .await
            .expect("write should succeed");

        assert_eq!(
            path,
            tmp.path()
                .join("crash_logs")
                .join(format!("{model_id}-{ts}.log"))
        );
        let read = tokio::fs::read(&path).await.expect("read");
        assert_eq!(read, payload);
        assert!(tmp.path().join("crash_logs").is_dir());
    }

    #[tokio::test]
    async fn write_crash_log_file_handles_existing_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("crash_logs");
        tokio::fs::create_dir_all(&dir).await.expect("pre-create");

        let path = write_crash_log_file(tmp.path(), "alpha", 42, b"first")
            .await
            .expect("first write");
        assert!(path.exists());

        let path2 = write_crash_log_file(tmp.path(), "beta", 99, b"second")
            .await
            .expect("second write");
        assert!(path2.exists());
        assert_ne!(path, path2);
    }

    #[tokio::test]
    async fn gc_under_cap_is_no_op() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("crash_logs");
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");

        for i in 0..3 {
            let p = dir.join(format!("m-{i}.log"));
            tokio::fs::write(&p, b"x").await.expect("write");
        }

        // Cap massively above the 3 bytes total — no eviction expected.
        gc_crash_logs(&dir, CRASH_LOGS_MAX_BYTES).await;

        let mut count = 0;
        let mut rd = tokio::fs::read_dir(&dir).await.expect("rd");
        while rd.next_entry().await.expect("entry").is_some() {
            count += 1;
        }
        assert_eq!(count, 3, "all files should survive when under cap");
    }

    #[tokio::test]
    async fn gc_evicts_oldest_first() {
        // Five .log files, each 100 bytes. Cap of 250 bytes ⇒ 3 oldest must
        // go (3 × 100 evicted, leaving 200 bytes < 250 cap held by 2 newest).
        // We sleep 50ms between writes to guarantee distinct mtimes.
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("crash_logs");
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");

        let payload = vec![b'x'; 100];
        let mut paths = Vec::new();
        for i in 0..5 {
            let p = dir.join(format!("m-{i}.log"));
            tokio::fs::write(&p, &payload).await.expect("write");
            paths.push(p);
            // Bump mtime resolution; ext4/tmpfs nanosecond mtimes don't
            // need this, but it's cheap insurance against coarse fs's.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        gc_crash_logs(&dir, 250).await;

        // Newest two should survive.
        assert!(!paths[0].exists(), "oldest #0 should be evicted");
        assert!(!paths[1].exists(), "next-oldest #1 should be evicted");
        assert!(!paths[2].exists(), "third-oldest #2 should be evicted");
        assert!(paths[3].exists(), "newer #3 should survive");
        assert!(paths[4].exists(), "newest #4 should survive");
    }

    #[tokio::test]
    async fn gc_handles_empty_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("crash_logs");
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");

        // No .log files; must not panic, must not create files.
        gc_crash_logs(&dir, 1024).await;

        let mut count = 0;
        let mut rd = tokio::fs::read_dir(&dir).await.expect("rd");
        while rd.next_entry().await.expect("entry").is_some() {
            count += 1;
        }
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn gc_handles_missing_dir() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("does-not-exist");
        // Must not panic when the directory has never been created.
        gc_crash_logs(&dir, 1024).await;
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn gc_ignores_non_log_files() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("crash_logs");
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let txt = dir.join("ignore-me.txt");
        tokio::fs::write(&txt, vec![b'x'; 10_000])
            .await
            .expect("write txt");

        gc_crash_logs(&dir, 100).await;

        // Despite cap being 100 and the .txt being 10_000 bytes, GC must
        // not touch non-.log files.
        assert!(txt.exists());
    }
}
