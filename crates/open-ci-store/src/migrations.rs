//! Normalized `SQLite` schema migrations for `open-ci-store`.

use rusqlite::Connection;

const MIGRATION_0001: &str = r"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS control_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    mode TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    reason TEXT NOT NULL
);

INSERT OR IGNORE INTO control_state (id, mode, updated_at, updated_by, reason)
VALUES (1, 'active', 0, 'system', 'initial');

CREATE TABLE IF NOT EXISTS pool_state (
    pool_id TEXT PRIMARY KEY,
    routing_cursor INTEGER NOT NULL DEFAULT 0,
    last_discovery_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS admissions (
    id TEXT PRIMARY KEY,
    repository TEXT NOT NULL,
    run_id INTEGER NOT NULL,
    run_attempt INTEGER NOT NULL,
    head_sha TEXT NOT NULL,
    pools_json TEXT NOT NULL,
    target TEXT NOT NULL,
    reason TEXT NOT NULL,
    status TEXT NOT NULL,
    vcpu INTEGER NOT NULL,
    instances INTEGER NOT NULL,
    disk_gb INTEGER NOT NULL,
    runners_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    released_at INTEGER,
    UNIQUE(repository, run_id, run_attempt)
);

CREATE INDEX IF NOT EXISTS idx_admissions_status ON admissions(status);
CREATE INDEX IF NOT EXISTS idx_admissions_created_at ON admissions(created_at);

CREATE TABLE IF NOT EXISTS workers (
    name TEXT PRIMARY KEY,
    pool_id TEXT NOT NULL,
    admission_id TEXT,
    request_id TEXT NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    placement_index INTEGER NOT NULL,
    region TEXT NOT NULL,
    zone TEXT NOT NULL,
    machine_type TEXT NOT NULL,
    provider_instance_id TEXT,
    phase TEXT NOT NULL,
    create_operation TEXT,
    delete_operation TEXT,
    delete_attempts INTEGER NOT NULL DEFAULT 0,
    bootstrap_used INTEGER NOT NULL DEFAULT 0,
    runner_id INTEGER,
    job_id INTEGER,
    retire_reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_workers_pool_phase ON workers(pool_id, phase);
CREATE INDEX IF NOT EXISTS idx_workers_admission ON workers(admission_id);

CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    pool_id TEXT NOT NULL,
    admission_id TEXT,
    run_id INTEGER,
    status TEXT NOT NULL,
    conclusion TEXT,
    runner_name TEXT,
    runner_id INTEGER,
    checked_at INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_jobs_pool_status ON jobs(pool_id, status);
CREATE INDEX IF NOT EXISTS idx_jobs_runner_name ON jobs(runner_name);

CREATE TABLE IF NOT EXISTS worker_usage (
    worker_name TEXT PRIMARY KEY,
    rx_bytes INTEGER NOT NULL,
    tx_bytes INTEGER NOT NULL,
    observed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS placement_backoffs (
    provider TEXT NOT NULL,
    region TEXT NOT NULL,
    family TEXT NOT NULL,
    retry_after INTEGER NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (provider, region, family)
);

CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    detail_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_log_timestamp ON audit_log(timestamp DESC);
";

pub fn apply(conn: &mut Connection, now_unix: u64) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;

    let tx = conn.transaction()?;
    tx.execute_batch(MIGRATION_0001)?;
    let now_i64 = i64::try_from(now_unix).unwrap_or(i64::MAX);
    tx.execute(
        "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
        [now_i64],
    )?;

    let v2_applied: i64 = tx.query_row(
        "SELECT COUNT(*) FROM schema_migrations WHERE version = 2",
        [],
        |r| r.get(0),
    )?;
    if v2_applied == 0 {
        tx.execute_batch("ALTER TABLE jobs ADD COLUMN repository TEXT;")?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (2, ?1)",
            [now_i64],
        )?;
    }

    tx.commit()?;
    Ok(())
}
