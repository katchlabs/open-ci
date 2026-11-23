//! In-memory and JSONL-serializable cache metadata index.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A committed cache entry scoped to a specific tenant/repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEntry {
    pub entry_id: u64,
    pub scope: String,
    pub key: String,
    pub version: String,
    pub blob_id: String,
    pub size_bytes: u64,
    pub created_at: u64,
    pub last_accessed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct EntryLookupKey {
    scope: String,
    key: String,
    version: String,
}

/// Pending upload reservation created by `CreateCacheEntry` and finalized by
/// `FinalizeCacheEntryUpload`.
#[derive(Clone, Debug)]
pub struct PendingUpload {
    pub blob_id: String,
    pub created_at: u64,
}

/// Scoped cache entry metadata index supporting exact matches and `restore_keys` prefix lookups.
#[derive(Debug, Default)]
pub struct CacheIndex {
    entries: HashMap<EntryLookupKey, CacheEntry>,
    pending: HashMap<EntryLookupKey, PendingUpload>,
    next_entry_id: u64,
}

impl CacheIndex {
    /// Constructs a [`CacheIndex`] hydrated from persisted entries.
    pub fn from_entries(entries: Vec<CacheEntry>) -> Self {
        let mut index = Self {
            entries: HashMap::new(),
            pending: HashMap::new(),
            next_entry_id: 1,
        };
        for entry in entries {
            if entry.entry_id >= index.next_entry_id {
                index.next_entry_id = entry.entry_id.saturating_add(1);
            }
            let lookup = EntryLookupKey {
                scope: entry.scope.clone(),
                key: entry.key.clone(),
                version: entry.version.clone(),
            };
            index.entries.insert(lookup, entry);
        }
        index
    }

    /// Registers a pending upload reservation for `(scope, key, version)`.
    pub fn register_pending(
        &mut self,
        scope: &str,
        key: &str,
        version: &str,
        blob_id: String,
        now_unix: u64,
    ) {
        let lookup = EntryLookupKey {
            scope: scope.to_owned(),
            key: key.to_owned(),
            version: version.to_owned(),
        };
        self.pending.insert(
            lookup,
            PendingUpload {
                blob_id,
                created_at: now_unix,
            },
        );
    }

    /// Consumes and returns a pending upload reservation for `(scope, key, version)`.
    pub fn take_pending(&mut self, scope: &str, key: &str, version: &str) -> Option<PendingUpload> {
        let lookup = EntryLookupKey {
            scope: scope.to_owned(),
            key: key.to_owned(),
            version: version.to_owned(),
        };
        self.pending.remove(&lookup)
    }

    /// Commits a finalized cache entry and returns the entry plus any displaced blob ID.
    pub fn commit_entry(
        &mut self,
        scope: &str,
        key: &str,
        version: &str,
        blob_id: String,
        size_bytes: u64,
        now_unix: u64,
    ) -> (CacheEntry, Option<String>) {
        if self.next_entry_id == 0 {
            self.next_entry_id = 1;
        }
        let entry_id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.saturating_add(1);

        let entry = CacheEntry {
            entry_id,
            scope: scope.to_owned(),
            key: key.to_owned(),
            version: version.to_owned(),
            blob_id,
            size_bytes,
            created_at: now_unix,
            last_accessed_at: now_unix,
        };
        let lookup = EntryLookupKey {
            scope: scope.to_owned(),
            key: key.to_owned(),
            version: version.to_owned(),
        };
        let replaced_blob = self
            .entries
            .insert(lookup, entry.clone())
            .filter(|old| old.blob_id != entry.blob_id)
            .map(|old| old.blob_id);
        (entry, replaced_blob)
    }

    /// Resolves candidates for `(scope, key, version)` and `restore_keys` in priority order.
    ///
    /// Order:
    /// 1. Exact `(scope, key, version)` match if present.
    /// 2. For each prefix in `restore_keys`, all matching `(scope, version)` entries sorted
    ///    newest-first by `(created_at, entry_id)`.
    pub fn find_candidates(
        &self,
        scope: &str,
        key: &str,
        restore_keys: &[String],
        version: &str,
    ) -> Vec<CacheEntry> {
        let mut results = Vec::new();
        let exact_key = EntryLookupKey {
            scope: scope.to_owned(),
            key: key.to_owned(),
            version: version.to_owned(),
        };
        if let Some(exact) = self.entries.get(&exact_key) {
            results.push(exact.clone());
        }

        for prefix in restore_keys {
            let mut matches: Vec<CacheEntry> = self
                .entries
                .values()
                .filter(|entry| {
                    entry.scope == scope
                        && entry.version == version
                        && entry.key.starts_with(prefix.as_str())
                        && entry.key != key
                })
                .cloned()
                .collect();
            matches.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.entry_id.cmp(&a.entry_id))
            });
            for item in matches {
                if !results
                    .iter()
                    .any(|existing| existing.entry_id == item.entry_id)
                {
                    results.push(item);
                }
            }
        }
        results
    }

    /// Updates `last_accessed_at` for a matched entry.
    pub fn touch(&mut self, scope: &str, key: &str, version: &str, now_unix: u64) {
        let lookup = EntryLookupKey {
            scope: scope.to_owned(),
            key: key.to_owned(),
            version: version.to_owned(),
        };
        if let Some(entry) = self.entries.get_mut(&lookup) {
            entry.last_accessed_at = now_unix;
        }
    }

    /// Removes an entry (e.g., if its backing blob was evicted).
    pub fn remove(&mut self, scope: &str, key: &str, version: &str) {
        let lookup = EntryLookupKey {
            scope: scope.to_owned(),
            key: key.to_owned(),
            version: version.to_owned(),
        };
        self.entries.remove(&lookup);
    }

    /// Snapshots all committed entries for persistence.
    pub fn snapshot(&self) -> Vec<CacheEntry> {
        let mut list: Vec<CacheEntry> = self.entries.values().cloned().collect();
        list.sort_by_key(|entry| entry.entry_id);
        list
    }
}

/// Encodes cache entries into newline-delimited JSON (JSONL).
pub fn encode_jsonl(entries: &[CacheEntry]) -> Result<Vec<u8>, serde_json::Error> {
    let mut out = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut out, entry)?;
        out.push(b'\n');
    }
    Ok(out)
}

/// Decodes cache entries from newline-delimited JSON, skipping blank or malformed lines.
pub fn decode_jsonl(raw: &str) -> Vec<CacheEntry> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<CacheEntry>(line).ok())
        .collect()
}
