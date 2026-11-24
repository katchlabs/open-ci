//! `CacheIndex` semantics: immutability, reservations, GitHub restore order, and retention.

use open_ci_cache::{BlobId, CacheIndex, EntryKey, Reservation, RestoreQuery, RetentionPolicy};
use proptest::prelude::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const REPO: u64 = 77;
const MAIN: &str = "refs/heads/main";
const PR: &str = "refs/pull/3/merge";
const DAY: u64 = 86_400;

fn key(scope: &str, version: &str, key: &str) -> EntryKey {
    EntryKey {
        repository_id: REPO,
        scope: scope.to_owned(),
        version: version.to_owned(),
        key: key.to_owned(),
    }
}

async fn save(
    index: &CacheIndex,
    entry: EntryKey,
    size: u64,
    now: u64,
) -> Result<BlobId, Box<dyn std::error::Error>> {
    let blob = BlobId::generate();
    let committed = index.commit(entry, blob.clone(), size, now).await?;
    assert!(committed.is_some(), "entry must be new");
    Ok(blob)
}

async fn restore(
    index: &CacheIndex,
    scopes: &[&str],
    primary: &str,
    restore_keys: &[&str],
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let restore_keys: Vec<String> = restore_keys.iter().map(|k| (*k).to_owned()).collect();
    let found = index
        .find(&RestoreQuery {
            repository_id: REPO,
            version: "v1",
            scopes,
            primary_key: primary,
            restore_keys: &restore_keys,
        })
        .await?;
    Ok(found.map(|e| format!("{}:{}", e.key.scope, e.key.key)))
}

#[tokio::test]
async fn entries_are_immutable_and_live_reservations_block_duplicates() -> TestResult {
    let index = CacheIndex::open_in_memory()?;
    let entry = key(MAIN, "v1", "deps-abc");

    let first = BlobId::generate();
    assert_eq!(
        index
            .reserve(entry.clone(), first.clone(), 100, 200)
            .await?,
        (Reservation::Reserved, None)
    );
    assert_eq!(
        index
            .reserve(entry.clone(), BlobId::generate(), 150, 250)
            .await?
            .0,
        Reservation::AlreadyExists,
        "a live reservation blocks a concurrent save"
    );
    let retry = BlobId::generate();
    assert_eq!(
        index
            .reserve(entry.clone(), retry.clone(), 200, 300)
            .await?,
        (Reservation::Reserved, Some(first)),
        "a stale reservation is replaced and its blob handed back for cleanup"
    );

    assert_eq!(
        index.take_reservation(entry.clone()).await?,
        Some(retry.clone())
    );
    assert_eq!(index.take_reservation(entry.clone()).await?, None);
    assert!(index.commit(entry.clone(), retry, 10, 210).await?.is_some());
    assert!(
        index
            .commit(entry.clone(), BlobId::generate(), 10, 220)
            .await?
            .is_none()
    );
    assert_eq!(
        index.reserve(entry, BlobId::generate(), 230, 330).await?.0,
        Reservation::AlreadyExists
    );
    Ok(())
}

#[tokio::test]
async fn restores_search_own_ref_before_base_then_exact_before_newest_prefix() -> TestResult {
    let index = CacheIndex::open_in_memory()?;
    save(&index, key(MAIN, "v1", "deps-linux-aaa"), 1, 10).await?;
    save(&index, key(MAIN, "v1", "deps-linux-bbb"), 1, 20).await?;
    save(&index, key(PR, "v1", "deps-linux-old"), 1, 5).await?;

    let pr_scopes = [PR, MAIN];
    assert_eq!(
        restore(&index, &pr_scopes, "deps-linux-aaa", &["deps-linux-"])
            .await?
            .as_deref(),
        Some("refs/pull/3/merge:deps-linux-old"),
        "any match in the job's own ref wins before the base ref is consulted"
    );
    assert_eq!(
        restore(&index, &[MAIN], "deps-linux-zzz", &["deps-linux-"])
            .await?
            .as_deref(),
        Some("refs/heads/main:deps-linux-bbb"),
        "prefix matches pick the newest entry"
    );
    assert_eq!(
        restore(&index, &[MAIN], "deps-linux-aaa", &["deps-linux-"])
            .await?
            .as_deref(),
        Some("refs/heads/main:deps-linux-aaa"),
        "an exact primary match beats a newer prefix match"
    );
    assert_eq!(restore(&index, &[MAIN], "other", &["nope-"]).await?, None);
    Ok(())
}

#[tokio::test]
async fn lookups_are_isolated_by_repository_version_and_unreadable_scope() -> TestResult {
    let index = CacheIndex::open_in_memory()?;
    save(&index, key(PR, "v1", "deps"), 1, 10).await?;
    save(&index, key(MAIN, "v2", "deps"), 1, 10).await?;
    let mut foreign = key(MAIN, "v1", "deps");
    foreign.repository_id = REPO + 1;
    save(&index, foreign, 1, 10).await?;

    assert_eq!(
        restore(&index, &[MAIN], "deps", &[]).await?,
        None,
        "main cannot see a PR's entry, another version, or another repository"
    );
    Ok(())
}

#[tokio::test]
async fn prefix_matching_treats_sql_wildcards_literally() -> TestResult {
    let index = CacheIndex::open_in_memory()?;
    save(&index, key(MAIN, "v1", "abXcd"), 1, 10).await?;
    assert_eq!(restore(&index, &[MAIN], "zz", &["a_"]).await?, None);
    assert_eq!(restore(&index, &[MAIN], "zz", &["ab%"]).await?, None);
    save(&index, key(MAIN, "v1", "a_%-literal"), 1, 20).await?;
    assert_eq!(
        restore(&index, &[MAIN], "zz", &["a_%"]).await?.as_deref(),
        Some("refs/heads/main:a_%-literal")
    );
    Ok(())
}

#[tokio::test]
async fn sweep_removes_stale_reservations_unused_entries_and_lru_overflow() -> TestResult {
    let index = CacheIndex::open_in_memory()?;
    let now = 100 * DAY;
    let stale = BlobId::generate();
    index
        .reserve(key(MAIN, "v1", "pending"), stale.clone(), now - 10, now - 1)
        .await?;
    let unused = save(&index, key(MAIN, "v1", "unused"), 10, now - 8 * DAY).await?;
    let lru = save(&index, key(MAIN, "v1", "lru"), 60, now - DAY).await?;
    let recent = save(&index, key(MAIN, "v1", "recent"), 60, now - 60).await?;

    let swept = index
        .sweep(
            now,
            RetentionPolicy {
                max_total_bytes: 100,
                max_unused_days: 7,
            },
        )
        .await?;

    assert_eq!(swept.blobs, vec![stale, unused, lru]);
    assert!(!swept.blobs.contains(&recent));
    assert_eq!(index.total_bytes().await?, 60);
    assert_eq!(
        restore(&index, &[MAIN], "recent", &[]).await?.as_deref(),
        Some("refs/heads/main:recent")
    );
    Ok(())
}

#[tokio::test]
async fn index_persists_across_reopen() -> TestResult {
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("nested").join("index.db");
    {
        let index = CacheIndex::open(&path).await?;
        save(&index, key(MAIN, "v1", "deps"), 5, 10).await?;
    }
    let reopened = CacheIndex::open(&path).await?;
    assert_eq!(
        restore(&reopened, &[MAIN], "deps", &[]).await?.as_deref(),
        Some("refs/heads/main:deps")
    );
    Ok(())
}

proptest! {
    #[test]
    fn lru_sweep_keeps_exactly_the_most_recently_used_entries_within_budget(
        sizes in proptest::collection::vec(1u64..1_000, 1..40),
        budget in 1u64..20_000,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().build()
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
        runtime.block_on(async {
            let index = CacheIndex::open_in_memory().map_err(|e| TestCaseError::fail(e.to_string()))?;
            let now = 10 * DAY;
            for (i, size) in sizes.iter().enumerate() {
                let accessed = now - 1_000 + i as u64;
                index.commit(key(MAIN, "v1", &format!("k{i}")), BlobId::generate(), *size, accessed)
                    .await.map_err(|e| TestCaseError::fail(e.to_string()))?;
            }
            index.sweep(now, RetentionPolicy { max_total_bytes: budget, max_unused_days: 7 })
                .await.map_err(|e| TestCaseError::fail(e.to_string()))?;

            let mut expected_kept = 0u64;
            for size in sizes.iter().rev() {
                if expected_kept + size > budget { break; }
                expected_kept += size;
            }
            let total = index.total_bytes().await.map_err(|e| TestCaseError::fail(e.to_string()))?;
            prop_assert!(total <= budget);
            prop_assert_eq!(total, expected_kept);
            Ok(())
        })?;
    }
}
