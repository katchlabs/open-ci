//! `SQLite` index of cache entries and in-flight upload reservations.
//!
//! Entries are immutable and keyed by `(repository, ref scope, version, key)`, matching GitHub:
//! a scope's key is saved once, and restores search scopes in the caller's read order.

use crate::error::CacheError;
use crate::ids::BlobId;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

const SECONDS_PER_DAY: u64 = 86_400;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entries (
    id               INTEGER PRIMARY KEY,
    repository_id    INTEGER NOT NULL,
    scope            TEXT    NOT NULL,
    version          TEXT    NOT NULL,
    key              TEXT    NOT NULL,
    blob_id          TEXT    NOT NULL UNIQUE,
    size_bytes       INTEGER NOT NULL,
    created_at       INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL,
    UNIQUE (repository_id, scope, version, key)
);
CREATE INDEX IF NOT EXISTS entries_by_last_access ON entries (last_accessed_at);
CREATE TABLE IF NOT EXISTS reservations (
    repository_id INTEGER NOT NULL,
    scope         TEXT    NOT NULL,
    version       TEXT    NOT NULL,
    key           TEXT    NOT NULL,
    blob_id       TEXT    NOT NULL UNIQUE,
    stale_after   INTEGER NOT NULL,
    PRIMARY KEY (repository_id, scope, version, key)
);
";

/// Identity of one immutable cache entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryKey {
    pub repository_id: u64,
    pub scope: String,
    pub version: String,
    pub key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    pub id: i64,
    pub key: EntryKey,
    pub blob: BlobId,
    pub size_bytes: u64,
    pub created_at: u64,
    pub last_accessed_at: u64,
}

/// A restore lookup: scopes in search order, then the primary key, then restore-key prefixes.
#[derive(Debug)]
pub struct RestoreQuery<'a> {
    pub repository_id: u64,
    pub version: &'a str,
    pub scopes: &'a [&'a str],
    pub primary_key: &'a str,
    pub restore_keys: &'a [String],
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reservation {
    Reserved,
    AlreadyExists,
}

/// Retention limits applied by [`CacheIndex::sweep`].
#[derive(Clone, Copy, Debug)]
pub struct RetentionPolicy {
    pub max_total_bytes: u64,
    pub max_unused_days: u32,
}

/// Rows removed by a sweep; their blobs must be deleted from the blob store.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub blobs: Vec<BlobId>,
}

#[derive(Clone, Debug)]
pub struct CacheIndex {
    conn: Arc<Mutex<Connection>>,
}

impl CacheIndex {
    pub async fn open(path: &Path) -> Result<Self, CacheError> {
        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, CacheError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    CacheError::Storage(format!("creating index directory: {err}"))
                })?;
            }
            let conn = Connection::open(&path)?;
            // The index is rebuildable cache metadata, so WAL with NORMAL sync trades the last
            // few commits on power loss for fewer fsyncs; entries lost that way are just misses.
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
            conn.execute_batch(SCHEMA)?;
            Ok(conn)
        })
        .await
        .map_err(|err| join_error(&err))??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, CacheError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, work: F) -> Result<T, CacheError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, CacheError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| CacheError::Internal("cache index lock poisoned".to_owned()))?;
            work(&mut guard)
        })
        .await
        .map_err(|err| join_error(&err))?
    }

    /// Reserves `key` for an upload into `blob`. Fails when the entry exists or another upload
    /// holds a live reservation; a stale reservation is replaced and its blob returned for cleanup.
    pub async fn reserve(
        &self,
        key: EntryKey,
        blob: BlobId,
        now: u64,
        stale_after: u64,
    ) -> Result<(Reservation, Option<BlobId>), CacheError> {
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            let committed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM entries
                     WHERE repository_id = ?1 AND scope = ?2 AND version = ?3 AND key = ?4)",
                params![sql_u64(key.repository_id)?, key.scope, key.version, key.key],
                |row| row.get(0),
            )?;
            if committed {
                return Ok((Reservation::AlreadyExists, None));
            }
            let existing: Option<(String, i64)> = tx
                .query_row(
                    "SELECT blob_id, stale_after FROM reservations
                     WHERE repository_id = ?1 AND scope = ?2 AND version = ?3 AND key = ?4",
                    params![sql_u64(key.repository_id)?, key.scope, key.version, key.key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((_, existing_stale_after)) = &existing
                && sql_u64(now)? < *existing_stale_after
            {
                return Ok((Reservation::AlreadyExists, None));
            }
            tx.execute(
                "INSERT OR REPLACE INTO reservations
                 (repository_id, scope, version, key, blob_id, stale_after)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    sql_u64(key.repository_id)?,
                    key.scope,
                    key.version,
                    key.key,
                    blob.as_str(),
                    sql_u64(stale_after)?
                ],
            )?;
            tx.commit()?;
            let replaced = existing
                .map(|(blob_id, _)| BlobId::parse(&blob_id))
                .transpose()?;
            Ok((Reservation::Reserved, replaced))
        })
        .await
    }

    /// Whether `blob` still belongs to a live, unfinalized reservation and may receive bytes.
    pub async fn accepts_upload(&self, blob: BlobId, now: u64) -> Result<bool, CacheError> {
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE blob_id = ?1 AND stale_after > ?2)",
                params![blob.as_str(), sql_u64(now)?],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Removes and returns the reservation for `key`, if any.
    pub async fn take_reservation(&self, key: EntryKey) -> Result<Option<BlobId>, CacheError> {
        self.with_conn(move |conn| {
            let blob: Option<String> = conn
                .query_row(
                    "DELETE FROM reservations
                     WHERE repository_id = ?1 AND scope = ?2 AND version = ?3 AND key = ?4
                     RETURNING blob_id",
                    params![sql_u64(key.repository_id)?, key.scope, key.version, key.key],
                    |row| row.get(0),
                )
                .optional()?;
            blob.map(|id| BlobId::parse(&id)).transpose()
        })
        .await
    }

    /// Publishes an uploaded blob as the entry for `key`; `None` if the entry already exists.
    pub async fn commit(
        &self,
        key: EntryKey,
        blob: BlobId,
        size_bytes: u64,
        now: u64,
    ) -> Result<Option<CacheEntry>, CacheError> {
        self.with_conn(move |conn| {
            let inserted = conn
                .execute(
                    "INSERT INTO entries
                     (repository_id, scope, version, key, blob_id, size_bytes, created_at, last_accessed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                     ON CONFLICT (repository_id, scope, version, key) DO NOTHING",
                    params![
                        sql_u64(key.repository_id)?,
                        key.scope,
                        key.version,
                        key.key,
                        blob.as_str(),
                        sql_u64(size_bytes)?,
                        sql_u64(now)?
                    ],
                )
                ?;
            if inserted == 0 {
                return Ok(None);
            }
            Ok(Some(CacheEntry {
                id: conn.last_insert_rowid(),
                key,
                blob,
                size_bytes,
                created_at: now,
                last_accessed_at: now,
            }))
        })
        .await
    }

    /// First match in GitHub order: per scope, per key (primary then restore keys), exact then
    /// newest prefix match.
    pub async fn find(&self, query: &RestoreQuery<'_>) -> Result<Option<CacheEntry>, CacheError> {
        let repository_id = sql_u64(query.repository_id)?;
        let version = query.version.to_owned();
        let scopes: Vec<String> = query.scopes.iter().map(|s| (*s).to_owned()).collect();
        let keys: Vec<String> = std::iter::once(query.primary_key.to_owned())
            .chain(query.restore_keys.iter().cloned())
            .collect();
        self.with_conn(move |conn| {
            let mut exact = conn.prepare_cached(FIND_EXACT)?;
            let mut prefix = conn.prepare_cached(FIND_NEWEST_PREFIX)?;
            for scope in &scopes {
                for key in &keys {
                    for statement in [&mut exact, &mut prefix] {
                        let found = statement
                            .query_row(params![repository_id, scope, version, key], EntryRow::read)
                            .optional()?;
                        if let Some(row) = found {
                            return CacheEntry::try_from(row).map(Some);
                        }
                    }
                }
            }
            Ok(None)
        })
        .await
    }

    pub async fn touch(&self, entry_id: i64, now: u64) -> Result<(), CacheError> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE entries SET last_accessed_at = ?2 WHERE id = ?1",
                params![entry_id, sql_u64(now)?],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn remove(&self, entry_id: i64) -> Result<(), CacheError> {
        self.with_conn(move |conn| {
            conn.execute("DELETE FROM entries WHERE id = ?1", params![entry_id])?;
            Ok(())
        })
        .await
    }

    pub async fn total_bytes(&self) -> Result<u64, CacheError> {
        self.with_conn(|conn| {
            let total: i64 = conn.query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM entries",
                [],
                |row| row.get(0),
            )?;
            from_sql_u64(total)
        })
        .await
    }

    /// Drops stale reservations, entries unused past retention, then least-recently-used entries
    /// until the total fits the budget.
    pub async fn sweep(&self, now: u64, policy: RetentionPolicy) -> Result<Swept, CacheError> {
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            let mut blobs = Vec::new();
            collect_blobs(
                &tx,
                "DELETE FROM reservations WHERE stale_after <= ?1 RETURNING blob_id",
                sql_u64(now)?,
                &mut blobs,
            )?;
            let unused_cutoff = now
                .saturating_sub(u64::from(policy.max_unused_days).saturating_mul(SECONDS_PER_DAY));
            collect_blobs(
                &tx,
                "DELETE FROM entries WHERE last_accessed_at < ?1 RETURNING blob_id",
                sql_u64(unused_cutoff)?,
                &mut blobs,
            )?;
            collect_blobs(
                &tx,
                "DELETE FROM entries WHERE id IN (
                    SELECT id FROM (
                        SELECT id, SUM(size_bytes) OVER (
                            ORDER BY last_accessed_at DESC, id DESC
                        ) AS newer_total
                        FROM entries
                    ) WHERE newer_total > ?1
                 ) RETURNING blob_id",
                sql_u64(policy.max_total_bytes)?,
                &mut blobs,
            )?;
            tx.commit()?;
            Ok(Swept { blobs })
        })
        .await
    }
}

const FIND_EXACT: &str = "SELECT id, repository_id, scope, version, key, blob_id, size_bytes,
    created_at, last_accessed_at FROM entries
    WHERE repository_id = ?1 AND scope = ?2 AND version = ?3 AND key = ?4";

const FIND_NEWEST_PREFIX: &str =
    "SELECT id, repository_id, scope, version, key, blob_id, size_bytes,
    created_at, last_accessed_at FROM entries
    WHERE repository_id = ?1 AND scope = ?2 AND version = ?3
      AND key >= ?4 AND substr(key, 1, length(?4)) = ?4
    ORDER BY created_at DESC, id DESC LIMIT 1";

struct EntryRow {
    id: i64,
    repository_id: i64,
    scope: String,
    version: String,
    key: String,
    blob_id: String,
    size_bytes: i64,
    created_at: i64,
    last_accessed_at: i64,
}

impl EntryRow {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            repository_id: row.get(1)?,
            scope: row.get(2)?,
            version: row.get(3)?,
            key: row.get(4)?,
            blob_id: row.get(5)?,
            size_bytes: row.get(6)?,
            created_at: row.get(7)?,
            last_accessed_at: row.get(8)?,
        })
    }
}

impl TryFrom<EntryRow> for CacheEntry {
    type Error = CacheError;

    fn try_from(row: EntryRow) -> Result<Self, CacheError> {
        Ok(Self {
            id: row.id,
            key: EntryKey {
                repository_id: from_sql_u64(row.repository_id)?,
                scope: row.scope,
                version: row.version,
                key: row.key,
            },
            blob: BlobId::parse(&row.blob_id)?,
            size_bytes: from_sql_u64(row.size_bytes)?,
            created_at: from_sql_u64(row.created_at)?,
            last_accessed_at: from_sql_u64(row.last_accessed_at)?,
        })
    }
}

fn collect_blobs(
    tx: &rusqlite::Transaction<'_>,
    sql: &str,
    bound: i64,
    blobs: &mut Vec<BlobId>,
) -> Result<(), CacheError> {
    let mut statement = tx.prepare(sql)?;
    let ids = statement.query_map(params![bound], |row| row.get::<_, String>(0))?;
    for id in ids {
        blobs.push(BlobId::parse(&id?)?);
    }
    Ok(())
}

fn sql_u64(value: u64) -> Result<i64, CacheError> {
    i64::try_from(value).map_err(|_| CacheError::BadRequest("value exceeds index range".to_owned()))
}

fn from_sql_u64(value: i64) -> Result<u64, CacheError> {
    u64::try_from(value)
        .map_err(|_| CacheError::Internal("negative value in cache index".to_owned()))
}

fn join_error(err: &tokio::task::JoinError) -> CacheError {
    CacheError::Internal(format!("cache index task failed: {err}"))
}
