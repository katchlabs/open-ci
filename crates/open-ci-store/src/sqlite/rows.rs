//! Row mappers and checked integer conversions for `SQLite` queries.

use crate::StoreError;
use open_ci_core::{
    AdmissionReason, AdmissionRecord, AdmissionStatus, Capacity, Job, JobStatus, ProviderKind,
    Worker, WorkerPhase,
};
use rusqlite::Row;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn u64_to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::CorruptData("u64 exceeds i64".to_owned()))
}

pub(super) fn i64_to_u64(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::CorruptData("negative integer in DB".to_owned()))
}

pub(super) fn i64_to_u32(value: i64, field: &str) -> Result<u32, StoreError> {
    u32::try_from(i64_to_u64(value)?)
        .map_err(|_| StoreError::CorruptData(format!("{field} overflow")))
}

pub(super) fn parse_provider(value: &str) -> Result<ProviderKind, StoreError> {
    match value {
        "gcp" => Ok(ProviderKind::Gcp),
        "aws" => Ok(ProviderKind::Aws),
        "mock" => Ok(ProviderKind::Mock),
        other => Err(StoreError::CorruptData(format!(
            "unknown provider `{other}`"
        ))),
    }
}

pub(super) fn parse_reason(value: &str) -> Result<AdmissionReason, StoreError> {
    match value {
        "admitted" => Ok(AdmissionReason::Admitted),
        "capacity" => Ok(AdmissionReason::Capacity),
        "unavailable" => Ok(AdmissionReason::Unavailable),
        "rerun" => Ok(AdmissionReason::Rerun),
        "no_work" => Ok(AdmissionReason::NoWork),
        "deadline" => Ok(AdmissionReason::Deadline),
        "daily_limit" => Ok(AdmissionReason::DailyLimit),
        "lifetime_limit" => Ok(AdmissionReason::LifetimeLimit),
        "control_mode" => Ok(AdmissionReason::ControlMode),
        other => Err(StoreError::CorruptData(format!(
            "unknown admission reason `{other}`"
        ))),
    }
}

pub(super) fn row_to_admission(row: &Row<'_>) -> Result<AdmissionRecord, StoreError> {
    let pools_json: String = row.get(5)?;
    let runners_json: String = row.get(12)?;
    let pools: BTreeSet<String> = serde_json::from_str(&pools_json)?;
    let runners: BTreeMap<String, String> = serde_json::from_str(&runners_json)?;
    let reason_str: String = row.get(7)?;
    let status_str: String = row.get(8)?;
    let status = AdmissionStatus::parse(&status_str)
        .ok_or_else(|| StoreError::CorruptData(format!("unknown status `{status_str}`")))?;
    let released_raw: Option<i64> = row.get(14)?;

    Ok(AdmissionRecord {
        id: row.get(0)?,
        repository: row.get(1)?,
        run_id: i64_to_u64(row.get(2)?)?,
        run_attempt: i64_to_u32(row.get(3)?, "run_attempt")?,
        head_sha: row.get(4)?,
        pools,
        target: row.get(6)?,
        reason: parse_reason(&reason_str)?,
        status,
        capacity: Capacity {
            vcpu: i64_to_u32(row.get(9)?, "vcpu")?,
            instances: i64_to_u32(row.get(10)?, "instances")?,
            disk_gb: i64_to_u32(row.get(11)?, "disk_gb")?,
        },
        runners,
        created_at: i64_to_u64(row.get(13)?)?,
        released_at: released_raw.map(i64_to_u64).transpose()?,
    })
}

pub(super) fn row_to_worker(row: &Row<'_>) -> Result<Worker, StoreError> {
    let provider_str: String = row.get(4)?;
    let phase_str: String = row.get(10)?;
    let phase = WorkerPhase::parse(&phase_str)
        .ok_or_else(|| StoreError::CorruptData(format!("unknown phase `{phase_str}`")))?;
    let runner_raw: Option<i64> = row.get(15)?;
    let job_raw: Option<i64> = row.get(16)?;

    Ok(Worker {
        name: row.get(0)?,
        pool_id: row.get(1)?,
        admission_id: row.get(2)?,
        request_id: row.get(3)?,
        provider: parse_provider(&provider_str)?,
        placement_index: usize::try_from(i64_to_u64(row.get(5)?)?)
            .map_err(|_| StoreError::CorruptData("placement_index overflow".to_owned()))?,
        region: row.get(6)?,
        zone: row.get(7)?,
        machine_type: row.get(8)?,
        provider_instance_id: row.get(9)?,
        phase,
        create_operation: row.get(11)?,
        delete_operation: row.get(12)?,
        delete_attempts: i64_to_u32(row.get(13)?, "delete_attempts")?,
        bootstrap_used: row.get::<_, i64>(14)? != 0,
        runner_id: runner_raw.map(i64_to_u64).transpose()?,
        job_id: job_raw.map(i64_to_u64).transpose()?,
        retire_reason: row.get(17)?,
        created_at: i64_to_u64(row.get(18)?)?,
        updated_at: i64_to_u64(row.get(19)?)?,
    })
}

pub(super) fn row_to_job(row: &Row<'_>) -> Result<Job, StoreError> {
    let run_raw: Option<i64> = row.get(4)?;
    let status_str: String = row.get(5)?;
    let runner_raw: Option<i64> = row.get(8)?;
    let checked_raw: Option<i64> = row.get(9)?;

    Ok(Job {
        id: i64_to_u64(row.get(0)?)?,
        pool_id: row.get(1)?,
        repository: row.get(2)?,
        admission_id: row.get(3)?,
        run_id: run_raw.map(i64_to_u64).transpose()?,
        status: JobStatus::parse(&status_str),
        conclusion: row.get(6)?,
        runner_name: row.get(7)?,
        runner_id: runner_raw.map(i64_to_u64).transpose()?,
        checked_at: checked_raw.map(i64_to_u64).transpose()?,
        updated_at: i64_to_u64(row.get(10)?)?,
    })
}
