//! Control-state, pool-state, and admission SQL queries.

use super::rows::{i64_to_u64, row_to_admission, u64_to_i64};
use crate::{ControlState, StoreError};
use open_ci_core::{AdmissionRecord, ControlMode};
use rusqlite::{Connection, OptionalExtension, params};

const ADMISSION_COLS: &str = "id, repository, run_id, run_attempt, head_sha, pools_json, target, \
    reason, status, vcpu, instances, disk_gb, runners_json, created_at, released_at";

pub(super) fn get_control_state(conn: &Connection) -> Result<ControlState, StoreError> {
    let (mode_str, updated_at, updated_by, reason): (String, i64, String, String) = conn
        .query_row(
            "SELECT mode, updated_at, updated_by, reason FROM control_state WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let mode = ControlMode::parse(&mode_str)
        .ok_or_else(|| StoreError::CorruptData(format!("unknown mode `{mode_str}`")))?;
    Ok(ControlState {
        mode,
        updated_at: i64_to_u64(updated_at)?,
        updated_by,
        reason,
    })
}

pub(super) fn set_control_mode(
    conn: &mut Connection,
    mode: ControlMode,
    actor: String,
    reason: String,
    now_unix: u64,
) -> Result<ControlState, StoreError> {
    let ts = u64_to_i64(now_unix)?;
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE control_state SET mode = ?1, updated_at = ?2, updated_by = ?3, reason = ?4 WHERE id = 1",
        params![mode.as_str(), ts, actor, reason],
    )?;
    let detail = serde_json::json!({ "mode": mode.as_str(), "reason": reason }).to_string();
    tx.execute(
        "INSERT INTO audit_log (timestamp, actor, action, target, detail_json) \
         VALUES (?1, ?2, 'set_control_mode', 'control_plane', ?3)",
        params![ts, actor, detail],
    )?;
    tx.commit()?;
    Ok(ControlState {
        mode,
        updated_at: now_unix,
        updated_by: actor,
        reason,
    })
}

pub(super) fn get_routing_cursor(conn: &Connection, pool_id: &str) -> Result<u64, StoreError> {
    let cursor: Option<i64> = conn
        .query_row(
            "SELECT routing_cursor FROM pool_state WHERE pool_id = ?1",
            [pool_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(cursor.map_or(0, |v| u64::try_from(v).unwrap_or(0)))
}

pub(super) fn set_routing_cursor(
    conn: &Connection,
    pool_id: &str,
    cursor: u64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO pool_state (pool_id, routing_cursor) VALUES (?1, ?2) \
         ON CONFLICT(pool_id) DO UPDATE SET routing_cursor = excluded.routing_cursor",
        params![pool_id, u64_to_i64(cursor)?],
    )?;
    Ok(())
}

pub(super) fn get_last_discovery(conn: &Connection, pool_id: &str) -> Result<u64, StoreError> {
    let ts: Option<i64> = conn
        .query_row(
            "SELECT last_discovery_at FROM pool_state WHERE pool_id = ?1",
            [pool_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(ts.map_or(0, |v| u64::try_from(v).unwrap_or(0)))
}

pub(super) fn set_last_discovery(
    conn: &Connection,
    pool_id: &str,
    timestamp: u64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO pool_state (pool_id, last_discovery_at) VALUES (?1, ?2) \
         ON CONFLICT(pool_id) DO UPDATE SET last_discovery_at = excluded.last_discovery_at",
        params![pool_id, u64_to_i64(timestamp)?],
    )?;
    Ok(())
}

pub(super) fn get_admission(
    conn: &Connection,
    id: &str,
) -> Result<Option<AdmissionRecord>, StoreError> {
    let sql = format!("SELECT {ADMISSION_COLS} FROM admissions WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([id])?;
    if let Some(row) = rows.next()? {
        return Ok(Some(row_to_admission(row)?));
    }
    Ok(None)
}

pub(super) fn get_admission_by_run(
    conn: &Connection,
    repository: &str,
    run_id: u64,
    run_attempt: u32,
) -> Result<Option<AdmissionRecord>, StoreError> {
    let sql = format!(
        "SELECT {ADMISSION_COLS} FROM admissions \
         WHERE repository = ?1 AND run_id = ?2 AND run_attempt = ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![
        repository,
        u64_to_i64(run_id)?,
        i64::from(run_attempt)
    ])?;
    if let Some(row) = rows.next()? {
        return Ok(Some(row_to_admission(row)?));
    }
    Ok(None)
}

pub(super) fn list_admissions(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<AdmissionRecord>, StoreError> {
    let limit_i64 = i64::try_from(limit).unwrap_or(100);
    let sql = format!("SELECT {ADMISSION_COLS} FROM admissions ORDER BY created_at DESC LIMIT ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([limit_i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_admission(row)?);
    }
    Ok(out)
}

pub(super) fn list_held_admissions(conn: &Connection) -> Result<Vec<AdmissionRecord>, StoreError> {
    let sql = format!(
        "SELECT {ADMISSION_COLS} FROM admissions WHERE status = 'held' ORDER BY created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(row_to_admission(row)?);
    }
    Ok(out)
}

pub(super) fn count_admitted(
    conn: &Connection,
    utc_day_start: u64,
) -> Result<(u32, u32), StoreError> {
    let day_start = u64_to_i64(utc_day_start)?;
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM admissions WHERE reason = 'admitted'",
        [],
        |r| r.get(0),
    )?;
    let daily: i64 = conn.query_row(
        "SELECT COUNT(*) FROM admissions WHERE reason = 'admitted' AND created_at >= ?1",
        [day_start],
        |r| r.get(0),
    )?;
    Ok((
        u32::try_from(total).unwrap_or(u32::MAX),
        u32::try_from(daily).unwrap_or(u32::MAX),
    ))
}

pub(super) fn save_admission(
    conn: &Connection,
    record: &AdmissionRecord,
) -> Result<(), StoreError> {
    let pools_json = serde_json::to_string(&record.pools)?;
    let runners_json = serde_json::to_string(&record.runners)?;
    let released = record.released_at.map(u64_to_i64).transpose()?;
    conn.execute(
        "INSERT OR REPLACE INTO admissions \
         (id, repository, run_id, run_attempt, head_sha, pools_json, target, \
          reason, status, vcpu, instances, disk_gb, runners_json, created_at, released_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            record.id,
            record.repository,
            u64_to_i64(record.run_id)?,
            i64::from(record.run_attempt),
            record.head_sha,
            pools_json,
            record.target,
            record.reason.as_str(),
            record.status.as_str(),
            i64::from(record.capacity.vcpu),
            i64::from(record.capacity.instances),
            i64::from(record.capacity.disk_gb),
            runners_json,
            u64_to_i64(record.created_at)?,
            released,
        ],
    )?;
    Ok(())
}

pub(super) fn release_admission(
    conn: &Connection,
    id: &str,
    released_at: u64,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE admissions SET status = 'released', released_at = ?1 WHERE id = ?2 AND status = 'held'",
        params![u64_to_i64(released_at)?, id],
    )?;
    Ok(())
}
