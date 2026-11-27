//! Worker, job, usage telemetry, backoff, and audit log SQL queries.

use super::rows::{i64_to_u64, parse_provider, row_to_job, row_to_worker, u64_to_i64};
use crate::StoreError;
use open_ci_core::{AuditEntry, Job, PlacementBackoff, Worker, WorkerUsage};
use rusqlite::{Connection, params};

const WORKER_COLS: &str = "name, pool_id, admission_id, request_id, provider, placement_index, \
    region, zone, machine_type, provider_instance_id, phase, create_operation, delete_operation, \
    delete_attempts, bootstrap_used, runner_id, job_id, retire_reason, created_at, updated_at";

const JOB_COLS: &str = "id, pool_id, repository, admission_id, run_id, status, conclusion, runner_name, \
    runner_id, checked_at, updated_at";

const PHASE_RANK_SQL: &str = "CASE {col} \
    WHEN 'planned' THEN 0 \
    WHEN 'creating' THEN 1 \
    WHEN 'live' THEN 2 \
    WHEN 'retiring' THEN 3 \
    WHEN 'gone' THEN 4 \
    ELSE 0 END";

const JOB_STATUS_RANK_SQL: &str = "CASE {col} \
    WHEN 'unknown' THEN 0 \
    WHEN 'queued' THEN 1 \
    WHEN 'in_progress' THEN 2 \
    WHEN 'completed' THEN 3 \
    ELSE 0 END";

pub(super) fn upsert_worker(conn: &Connection, w: &Worker) -> Result<(), StoreError> {
    let cur_rank = PHASE_RANK_SQL.replace("{col}", "workers.phase");
    let new_rank = PHASE_RANK_SQL.replace("{col}", "excluded.phase");
    let sql = format!(
        "INSERT INTO workers ({WORKER_COLS}) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20) \
         ON CONFLICT(name) DO UPDATE SET \
            pool_id = excluded.pool_id, \
            admission_id = COALESCE(workers.admission_id, excluded.admission_id), \
            provider_instance_id = COALESCE(excluded.provider_instance_id, workers.provider_instance_id), \
            phase = CASE WHEN ({new_rank}) >= ({cur_rank}) THEN excluded.phase ELSE workers.phase END, \
            create_operation = CASE WHEN ({new_rank}) >= ({cur_rank}) THEN excluded.create_operation ELSE workers.create_operation END, \
            delete_operation = COALESCE(excluded.delete_operation, workers.delete_operation), \
            delete_attempts = MAX(workers.delete_attempts, excluded.delete_attempts), \
            bootstrap_used = MAX(workers.bootstrap_used, excluded.bootstrap_used), \
            runner_id = CASE WHEN ({new_rank}) > ({cur_rank}) THEN excluded.runner_id ELSE COALESCE(excluded.runner_id, workers.runner_id) END, \
            job_id = COALESCE(excluded.job_id, workers.job_id), \
            retire_reason = COALESCE(excluded.retire_reason, workers.retire_reason), \
            updated_at = MAX(workers.updated_at, excluded.updated_at)"
    );
    conn.execute(
        &sql,
        params![
            w.name,
            w.pool_id,
            w.admission_id,
            w.request_id,
            w.provider.as_str(),
            i64::try_from(w.placement_index)
                .map_err(|_| StoreError::CorruptData("placement_index".to_owned()))?,
            w.region,
            w.zone,
            w.machine_type,
            w.provider_instance_id,
            w.phase.as_str(),
            w.create_operation,
            w.delete_operation,
            i64::from(w.delete_attempts),
            i64::from(w.bootstrap_used),
            w.runner_id.map(u64_to_i64).transpose()?,
            w.job_id.map(u64_to_i64).transpose()?,
            w.retire_reason,
            u64_to_i64(w.created_at)?,
            u64_to_i64(w.updated_at)?,
        ],
    )?;
    Ok(())
}

pub(super) fn get_worker(conn: &Connection, name: &str) -> Result<Option<Worker>, StoreError> {
    let sql = format!("SELECT {WORKER_COLS} FROM workers WHERE name = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([name])?;
    if let Some(row) = rows.next()? {
        return Ok(Some(row_to_worker(row)?));
    }
    Ok(None)
}

pub(super) fn list_workers(
    conn: &Connection,
    pool_id: Option<&str>,
) -> Result<Vec<Worker>, StoreError> {
    let mut out = Vec::new();
    if let Some(pool) = pool_id {
        let sql = format!(
            "SELECT {WORKER_COLS} FROM workers WHERE pool_id = ?1 ORDER BY created_at DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([pool])?;
        while let Some(row) = rows.next()? {
            out.push(row_to_worker(row)?);
        }
    } else {
        let sql = format!("SELECT {WORKER_COLS} FROM workers ORDER BY created_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            out.push(row_to_worker(row)?);
        }
    }
    Ok(out)
}

pub(super) fn count_pool_workers(conn: &Connection, pool_id: &str) -> Result<usize, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM workers WHERE pool_id = ?1",
        [pool_id],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(0))
}

pub(super) fn consume_bootstrap(
    conn: &mut Connection,
    name: &str,
    now_unix: u64,
) -> Result<Worker, StoreError> {
    let ts = u64_to_i64(now_unix)?;
    let tx = conn.transaction()?;
    let updated = tx.execute(
        "UPDATE workers SET phase = 'live', create_operation = NULL, bootstrap_used = 1, updated_at = ?1 \
         WHERE name = ?2 AND phase IN ('creating', 'live') AND bootstrap_used = 0",
        params![ts, name],
    )?;
    if updated != 1 {
        return Err(StoreError::BootstrapUnavailable);
    }
    let worker = {
        let sql = format!("SELECT {WORKER_COLS} FROM workers WHERE name = ?1");
        let mut stmt = tx.prepare(&sql)?;
        let mut rows = stmt.query([name])?;
        let Some(row) = rows.next()? else {
            return Err(StoreError::BootstrapUnavailable);
        };
        row_to_worker(row)?
    };
    tx.commit()?;
    Ok(worker)
}

pub(super) fn observe_job_hint(
    conn: &Connection,
    job_id: u64,
    pool_id: &str,
    repository: Option<&str>,
    admission_id: Option<&str>,
    run_id: Option<u64>,
    now_unix: u64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO jobs (id, pool_id, repository, admission_id, run_id, status, checked_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'unknown', NULL, ?6) \
         ON CONFLICT(id) DO UPDATE SET \
            repository = COALESCE(excluded.repository, jobs.repository), \
            admission_id = COALESCE(jobs.admission_id, excluded.admission_id), \
            checked_at = NULL, \
            updated_at = excluded.updated_at",
        params![
            u64_to_i64(job_id)?,
            pool_id,
            repository,
            admission_id,
            run_id.map(u64_to_i64).transpose()?,
            u64_to_i64(now_unix)?,
        ],
    )?;
    Ok(())
}

pub(super) fn upsert_job(conn: &Connection, j: &Job) -> Result<(), StoreError> {
    let cur_rank = JOB_STATUS_RANK_SQL.replace("{col}", "jobs.status");
    let new_rank = JOB_STATUS_RANK_SQL.replace("{col}", "excluded.status");
    let sql = format!(
        "INSERT INTO jobs \
         (id, pool_id, repository, admission_id, run_id, status, conclusion, runner_name, runner_id, checked_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT(id) DO UPDATE SET \
            pool_id = excluded.pool_id, \
            repository = COALESCE(excluded.repository, jobs.repository), \
            admission_id = COALESCE(jobs.admission_id, excluded.admission_id), \
            run_id = COALESCE(excluded.run_id, jobs.run_id), \
            status = CASE WHEN ({new_rank}) >= ({cur_rank}) THEN excluded.status ELSE jobs.status END, \
            conclusion = COALESCE(excluded.conclusion, jobs.conclusion), \
            runner_name = COALESCE(excluded.runner_name, jobs.runner_name), \
            runner_id = COALESCE(excluded.runner_id, jobs.runner_id), \
            checked_at = COALESCE(excluded.checked_at, jobs.checked_at), \
            updated_at = MAX(jobs.updated_at, excluded.updated_at)"
    );
    conn.execute(
        &sql,
        params![
            u64_to_i64(j.id)?,
            j.pool_id,
            j.repository,
            j.admission_id,
            j.run_id.map(u64_to_i64).transpose()?,
            j.status.as_str(),
            j.conclusion,
            j.runner_name,
            j.runner_id.map(u64_to_i64).transpose()?,
            j.checked_at.map(u64_to_i64).transpose()?,
            u64_to_i64(j.updated_at)?,
        ],
    )?;
    Ok(())
}

pub(super) fn list_jobs(conn: &Connection, pool_id: Option<&str>) -> Result<Vec<Job>, StoreError> {
    let mut out = Vec::new();
    if let Some(pool) = pool_id {
        let sql =
            format!("SELECT {JOB_COLS} FROM jobs WHERE pool_id = ?1 ORDER BY updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([pool])?;
        while let Some(row) = rows.next()? {
            out.push(row_to_job(row)?);
        }
    } else {
        let sql = format!("SELECT {JOB_COLS} FROM jobs ORDER BY updated_at DESC");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            out.push(row_to_job(row)?);
        }
    }
    Ok(out)
}

pub(super) fn record_usage(
    conn: &Connection,
    worker_name: &str,
    usage: WorkerUsage,
    observed_at: u64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO worker_usage (worker_name, rx_bytes, tx_bytes, observed_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            worker_name,
            u64_to_i64(usage.rx_bytes)?,
            u64_to_i64(usage.tx_bytes)?,
            u64_to_i64(observed_at)?,
        ],
    )?;
    Ok(())
}

pub(super) fn list_usage(conn: &Connection) -> Result<Vec<(String, WorkerUsage, u64)>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT worker_name, rx_bytes, tx_bytes, observed_at FROM worker_usage ORDER BY observed_at DESC",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((
            row.get(0)?,
            WorkerUsage {
                rx_bytes: i64_to_u64(row.get(1)?)?,
                tx_bytes: i64_to_u64(row.get(2)?)?,
            },
            i64_to_u64(row.get(3)?)?,
        ));
    }
    Ok(out)
}

pub(super) fn upsert_backoff(conn: &Connection, b: &PlacementBackoff) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO placement_backoffs (provider, region, family, retry_after, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(provider, region, family) DO UPDATE SET \
            retry_after = MAX(placement_backoffs.retry_after, excluded.retry_after), \
            reason = excluded.reason",
        params![
            b.provider.as_str(),
            b.region,
            b.family,
            u64_to_i64(b.retry_after)?,
            b.reason,
        ],
    )?;
    Ok(())
}

pub(super) fn list_active_backoffs(
    conn: &Connection,
    now_unix: u64,
) -> Result<Vec<PlacementBackoff>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT provider, region, family, retry_after, reason \
         FROM placement_backoffs WHERE retry_after > ?1 ORDER BY retry_after ASC",
    )?;
    let mut rows = stmt.query([u64_to_i64(now_unix)?])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let provider_str: String = row.get(0)?;
        out.push(PlacementBackoff {
            provider: parse_provider(&provider_str)?,
            region: row.get(1)?,
            family: row.get(2)?,
            retry_after: i64_to_u64(row.get(3)?)?,
            reason: row.get(4)?,
        });
    }
    Ok(out)
}

pub(super) fn append_audit(
    conn: &Connection,
    timestamp: u64,
    actor: &str,
    action: &str,
    target: &str,
    detail_json: &str,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO audit_log (timestamp, actor, action, target, detail_json) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![u64_to_i64(timestamp)?, actor, action, target, detail_json],
    )?;
    Ok(())
}

pub(super) fn list_audit(conn: &Connection, limit: usize) -> Result<Vec<AuditEntry>, StoreError> {
    let limit_i64 = i64::try_from(limit).unwrap_or(100);
    let mut stmt = conn.prepare(
        "SELECT id, timestamp, actor, action, target, detail_json \
         FROM audit_log ORDER BY id DESC LIMIT ?1",
    )?;
    let mut rows = stmt.query([limit_i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let detail_json: String = row.get(5)?;
        out.push(AuditEntry {
            id: row.get(0)?,
            timestamp: i64_to_u64(row.get(1)?)?,
            actor: row.get(2)?,
            action: row.get(3)?,
            target: row.get(4)?,
            detail: serde_json::from_str(&detail_json)?,
        });
    }
    Ok(out)
}
