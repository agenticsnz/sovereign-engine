-- Supervisor schema (Phase 1 of backend supervisor feature).
--
-- Adds three columns to `models` and a new `backend_crash_log` table.
--
-- `worked` is the persisted half of the hybrid in-memory/DB worked flag.
-- It survives proxy restarts so the supervisor can decide quarantine vs
-- restart even when `recover_gate_state` lost the in-memory map.
-- Stored as INTEGER (0/1) — SQLite has no real BOOLEAN, matching the
-- existing `loaded` column convention (see 20260212000000_initial.sql:29).
--
-- `quarantined_at` / `quarantine_reason` persist the quarantine state so
-- a never-worked-then-crashed model stays quarantined across restarts.
ALTER TABLE models ADD COLUMN worked INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models ADD COLUMN quarantined_at TEXT NULL;
ALTER TABLE models ADD COLUMN quarantine_reason TEXT NULL;

-- Crash diagnostics ledger. One row per detected crash.
-- log_path may point at a GC'd file (1 GiB cap on data/crash_logs/);
-- the UI handles "file no longer available" gracefully.
CREATE TABLE backend_crash_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    model_id TEXT NOT NULL,
    container_id TEXT,
    occurred_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    exit_code INTEGER,
    oom_killed INTEGER NOT NULL DEFAULT 0,
    signal TEXT,
    log_path TEXT,
    FOREIGN KEY (model_id) REFERENCES models(id) ON DELETE CASCADE
);

CREATE INDEX idx_backend_crash_log_model_id_occurred_at
    ON backend_crash_log(model_id, occurred_at DESC);
