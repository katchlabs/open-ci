//! Non-blocking `SQLite` implementation of [`crate::Store`].
//!
//! All `rusqlite` calls execute inside `tokio::task::spawn_blocking` so fsyncs
//! and WAL checkpoints never block Tokio async worker threads.

mod admissions;
mod rows;
mod workers;

use crate::{ControlState, Store, StoreError, migrations};
use async_trait::async_trait;
use fs2::FileExt;
use open_ci_core::{
    AdmissionRecord, AuditEntry, ControlMode, Job, PlacementBackoff, Worker, WorkerUsage,
};
use rusqlite::{Connection, OpenFlags};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[derive(Debug)]
struct Inner {
    conn: Mutex<Connection>,
    _lock_file: File,
}

/// Durable `SQLite` store with exclusive lockfile fencing and online backup support.
#[derive(Clone, Debug)]
pub struct SqliteStore {
    inner: Arc<Inner>,
}

impl SqliteStore {
    /// Opens or creates the `SQLite` database at `path` with `0o600` permissions
    /// before `SQLite` touches it (eliminating permission TOCTOU).
    pub fn open(path: &Path, now_unix: u64) -> Result<Self, StoreError> {
        ensure_parent_dir(path)?;
        let lock_path = path.with_extension("lock");
        let lock_file = open_private_file(&lock_path)?;
        lock_file
            .try_lock_exclusive()
            .map_err(|_| StoreError::AlreadyLocked)?;

        let _ = open_private_file(path)?;
        let mut conn = Connection::open(path)?;
        migrations::apply(&mut conn, now_unix)?;
        enforce_db_permissions(path);

        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                _lock_file: lock_file,
            }),
        })
    }

    /// Creates a consistent online `SQLite` backup from `source` to `destination`
    /// using a read-only connection — safe to invoke while a live `open-ci serve`
    /// process holds the exclusive writer lockfile.
    pub async fn backup_file_online(
        source: PathBuf,
        destination: PathBuf,
    ) -> Result<(), StoreError> {
        tokio::task::spawn_blocking(move || {
            let src = Connection::open_with_flags(
                &source,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            src.busy_timeout(Duration::from_secs(5))?;
            copy_online_backup(&src, &destination)
        })
        .await
        .map_err(|e| StoreError::Join(e.to_string()))?
    }

    async fn run<T, F>(&self, op: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner
                .conn
                .lock()
                .map_err(|_| StoreError::CorruptData("sqlite mutex poisoned".to_owned()))?;
            op(&mut guard)
        })
        .await
        .map_err(|e| StoreError::Join(e.to_string()))?
    }
}

fn ensure_parent_dir(path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn open_private_file(path: &Path) -> Result<File, std::io::Error> {
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    opts.mode(0o600);
    let file = opts.open(path)?;
    enforce_private_mode(path);
    Ok(file)
}

fn enforce_db_permissions(path: &Path) {
    enforce_private_mode(path);
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    enforce_private_mode(Path::new(&wal));
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    enforce_private_mode(Path::new(&shm));
}

#[cfg(unix)]
fn enforce_private_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(not(unix))]
fn enforce_private_mode(_path: &Path) {}

fn copy_online_backup(src: &Connection, destination: &Path) -> Result<(), StoreError> {
    ensure_parent_dir(destination)?;
    let tmp_name = format!(
        ".{}.{}.tmp",
        destination
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("backup"),
        std::process::id()
    );
    let tmp_path = destination.with_file_name(tmp_name);
    let _ = open_private_file(&tmp_path)?;
    {
        let mut dest = Connection::open(&tmp_path)?;
        let backup = rusqlite::backup::Backup::new(src, &mut dest)?;
        backup.run_to_completion(128, Duration::from_millis(10), None)?;
    }
    enforce_private_mode(&tmp_path);
    std::fs::rename(&tmp_path, destination)?;
    Ok(())
}

#[async_trait]
impl Store for SqliteStore {
    async fn control_state(&self) -> Result<ControlState, StoreError> {
        self.run(|conn| admissions::get_control_state(conn)).await
    }

    async fn set_control_mode(
        &self,
        mode: ControlMode,
        actor: String,
        reason: String,
        now_unix: u64,
    ) -> Result<ControlState, StoreError> {
        self.run(move |conn| admissions::set_control_mode(conn, mode, actor, reason, now_unix))
            .await
    }

    async fn get_routing_cursor(&self, pool_id: &str) -> Result<u64, StoreError> {
        let pool_id = pool_id.to_owned();
        self.run(move |conn| admissions::get_routing_cursor(conn, &pool_id))
            .await
    }

    async fn set_routing_cursor(&self, pool_id: &str, cursor: u64) -> Result<(), StoreError> {
        let pool_id = pool_id.to_owned();
        self.run(move |conn| admissions::set_routing_cursor(conn, &pool_id, cursor))
            .await
    }

    async fn get_last_discovery(&self, pool_id: &str) -> Result<u64, StoreError> {
        let pool_id = pool_id.to_owned();
        self.run(move |conn| admissions::get_last_discovery(conn, &pool_id))
            .await
    }

    async fn set_last_discovery(&self, pool_id: &str, timestamp: u64) -> Result<(), StoreError> {
        let pool_id = pool_id.to_owned();
        self.run(move |conn| admissions::set_last_discovery(conn, &pool_id, timestamp))
            .await
    }

    async fn get_admission(&self, id: &str) -> Result<Option<AdmissionRecord>, StoreError> {
        let id = id.to_owned();
        self.run(move |conn| admissions::get_admission(conn, &id))
            .await
    }

    async fn get_admission_by_run(
        &self,
        repository: &str,
        run_id: u64,
        run_attempt: u32,
    ) -> Result<Option<AdmissionRecord>, StoreError> {
        let repository = repository.to_owned();
        self.run(move |conn| {
            admissions::get_admission_by_run(conn, &repository, run_id, run_attempt)
        })
        .await
    }

    async fn list_admissions(&self, limit: usize) -> Result<Vec<AdmissionRecord>, StoreError> {
        self.run(move |conn| admissions::list_admissions(conn, limit))
            .await
    }

    async fn list_held_admissions(&self) -> Result<Vec<AdmissionRecord>, StoreError> {
        self.run(|conn| admissions::list_held_admissions(conn))
            .await
    }

    async fn count_admitted(&self, utc_day_start: u64) -> Result<(u32, u32), StoreError> {
        self.run(move |conn| admissions::count_admitted(conn, utc_day_start))
            .await
    }

    async fn save_admission(&self, record: &AdmissionRecord) -> Result<(), StoreError> {
        let record = record.clone();
        self.run(move |conn| admissions::save_admission(conn, &record))
            .await
    }

    async fn release_admission(&self, id: &str, released_at: u64) -> Result<(), StoreError> {
        let id = id.to_owned();
        self.run(move |conn| admissions::release_admission(conn, &id, released_at))
            .await
    }

    async fn upsert_worker(&self, worker: &Worker) -> Result<(), StoreError> {
        let worker = worker.clone();
        self.run(move |conn| workers::upsert_worker(conn, &worker))
            .await
    }

    async fn get_worker(&self, name: &str) -> Result<Option<Worker>, StoreError> {
        let name = name.to_owned();
        self.run(move |conn| workers::get_worker(conn, &name)).await
    }

    async fn list_workers(&self, pool_id: Option<&str>) -> Result<Vec<Worker>, StoreError> {
        let pool_id = pool_id.map(str::to_owned);
        self.run(move |conn| workers::list_workers(conn, pool_id.as_deref()))
            .await
    }

    async fn count_pool_workers(&self, pool_id: &str) -> Result<usize, StoreError> {
        let pool_id = pool_id.to_owned();
        self.run(move |conn| workers::count_pool_workers(conn, &pool_id))
            .await
    }

    async fn consume_bootstrap(&self, name: &str, now_unix: u64) -> Result<Worker, StoreError> {
        let name = name.to_owned();
        self.run(move |conn| workers::consume_bootstrap(conn, &name, now_unix))
            .await
    }

    async fn observe_job_hint(
        &self,
        job_id: u64,
        pool_id: &str,
        repository: Option<&str>,
        admission_id: Option<&str>,
        run_id: Option<u64>,
        now_unix: u64,
    ) -> Result<(), StoreError> {
        let pool_id = pool_id.to_owned();
        let repository = repository.map(str::to_owned);
        let admission_id = admission_id.map(str::to_owned);
        self.run(move |conn| {
            workers::observe_job_hint(
                conn,
                job_id,
                &pool_id,
                repository.as_deref(),
                admission_id.as_deref(),
                run_id,
                now_unix,
            )
        })
        .await
    }

    async fn upsert_job(&self, job: &Job) -> Result<(), StoreError> {
        let job = job.clone();
        self.run(move |conn| workers::upsert_job(conn, &job)).await
    }

    async fn list_jobs(&self, pool_id: Option<&str>) -> Result<Vec<Job>, StoreError> {
        let pool_id = pool_id.map(str::to_owned);
        self.run(move |conn| workers::list_jobs(conn, pool_id.as_deref()))
            .await
    }

    async fn record_usage(
        &self,
        worker_name: &str,
        usage: WorkerUsage,
        observed_at: u64,
    ) -> Result<(), StoreError> {
        let worker_name = worker_name.to_owned();
        self.run(move |conn| workers::record_usage(conn, &worker_name, usage, observed_at))
            .await
    }

    async fn list_usage(&self) -> Result<Vec<(String, WorkerUsage, u64)>, StoreError> {
        self.run(|conn| workers::list_usage(conn)).await
    }

    async fn upsert_backoff(&self, backoff: &PlacementBackoff) -> Result<(), StoreError> {
        let backoff = backoff.clone();
        self.run(move |conn| workers::upsert_backoff(conn, &backoff))
            .await
    }

    async fn list_active_backoffs(
        &self,
        now_unix: u64,
    ) -> Result<Vec<PlacementBackoff>, StoreError> {
        self.run(move |conn| workers::list_active_backoffs(conn, now_unix))
            .await
    }

    async fn append_audit(
        &self,
        timestamp: u64,
        actor: &str,
        action: &str,
        target: &str,
        detail: serde_json::Value,
    ) -> Result<(), StoreError> {
        let actor = actor.to_owned();
        let action = action.to_owned();
        let target = target.to_owned();
        let detail_json = serde_json::to_string(&detail)?;
        self.run(move |conn| {
            workers::append_audit(conn, timestamp, &actor, &action, &target, &detail_json)
        })
        .await
    }

    async fn list_audit(&self, limit: usize) -> Result<Vec<AuditEntry>, StoreError> {
        self.run(move |conn| workers::list_audit(conn, limit)).await
    }

    async fn backup_to(&self, destination: PathBuf) -> Result<(), StoreError> {
        self.run(move |conn| copy_online_backup(conn, &destination))
            .await
    }
}
