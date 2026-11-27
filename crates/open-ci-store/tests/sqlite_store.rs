//! Integration tests for `SqliteStore` locking, atomic bootstrap, and live online backup.

use open_ci_core::{ControlMode, ProviderKind, Worker, WorkerPhase};
use open_ci_store::{SqliteStore, Store, StoreError};
use tempfile::tempdir;

#[tokio::test]
async fn atomic_bootstrap_consumption_allows_only_single_use() {
    let dir = tempdir().ok();
    assert!(dir.is_some());
    if let Some(dir) = dir {
        let store = SqliteStore::open(&dir.path().join("open-ci.db"), 1000).ok();
        assert!(store.is_some());
        if let Some(store) = store {
            let worker = Worker {
                name: "open-ci-default-1".to_owned(),
                pool_id: "default".to_owned(),
                admission_id: None,
                request_id: "req-1".to_owned(),
                provider: ProviderKind::Mock,
                placement_index: 0,
                region: "local-1".to_owned(),
                zone: "local-1a".to_owned(),
                machine_type: "mock-standard-2".to_owned(),
                provider_instance_id: Some("mock-1".to_owned()),
                phase: WorkerPhase::Live,
                create_operation: None,
                delete_operation: None,
                delete_attempts: 0,
                bootstrap_used: false,
                runner_id: None,
                job_id: None,
                retire_reason: None,
                created_at: 1000,
                updated_at: 1000,
            };
            assert!(store.upsert_worker(&worker).await.is_ok());

            let first = store.consume_bootstrap("open-ci-default-1", 1010).await;
            assert!(first.is_ok());

            // Simulate a concurrent reconciler writing back a stale snapshot where bootstrap_used=false.
            assert!(store.upsert_worker(&worker).await.is_ok());
            let reloaded = store.get_worker("open-ci-default-1").await.ok().flatten();
            assert!(reloaded.is_some_and(|w| w.bootstrap_used));

            let second = store.consume_bootstrap("open-ci-default-1", 1011).await;
            assert!(matches!(second, Err(StoreError::BootstrapUnavailable)));

            let creating = Worker {
                name: "open-ci-default-creating".to_owned(),
                request_id: "req-creating".to_owned(),
                phase: WorkerPhase::Creating,
                ..worker.clone()
            };
            assert!(store.upsert_worker(&creating).await.is_ok());
            let promoted = store
                .consume_bootstrap("open-ci-default-creating", 1012)
                .await;
            assert!(matches!(
                promoted,
                Ok(w) if w.phase == WorkerPhase::Live && w.bootstrap_used
            ));
            let replay = store
                .consume_bootstrap("open-ci-default-creating", 1013)
                .await;
            assert!(matches!(replay, Err(StoreError::BootstrapUnavailable)));

            let planned = Worker {
                name: "open-ci-default-planned".to_owned(),
                request_id: "req-planned".to_owned(),
                phase: WorkerPhase::Planned,
                ..worker
            };
            assert!(store.upsert_worker(&planned).await.is_ok());
            assert!(matches!(
                store
                    .consume_bootstrap("open-ci-default-planned", 1014)
                    .await,
                Err(StoreError::BootstrapUnavailable)
            ));
        }
    }
}

#[tokio::test]
async fn online_backup_works_while_writer_holds_exclusive_lock() {
    let dir = tempdir().ok();
    assert!(dir.is_some());
    if let Some(dir) = dir {
        let db_path = dir.path().join("open-ci.db");
        let backup_path = dir.path().join("snapshot.db");
        let store = SqliteStore::open(&db_path, 1000).ok();
        assert!(store.is_some());
        if let Some(store) = store {
            assert!(
                store
                    .set_control_mode(
                        ControlMode::FallbackOnly,
                        "operator".to_owned(),
                        "maintenance".to_owned(),
                        1050,
                    )
                    .await
                    .is_ok()
            );

            // Live backup succeeds while `store` still holds the exclusive writer lock!
            let res = SqliteStore::backup_file_online(db_path.clone(), backup_path.clone()).await;
            assert!(res.is_ok());

            let restored = SqliteStore::open(&backup_path, 1060).ok();
            assert!(restored.is_some());
            if let Some(restored) = restored {
                let state = restored.control_state().await.ok();
                assert!(matches!(
                    state,
                    Some(s) if s.mode == ControlMode::FallbackOnly && s.reason == "maintenance"
                ));
            }
        }
    }
}

#[tokio::test]
async fn exclusive_lock_rejects_second_controller_instance() {
    let dir = tempdir().ok();
    assert!(dir.is_some());
    if let Some(dir) = dir {
        let db_path = dir.path().join("open-ci.db");
        let first = SqliteStore::open(&db_path, 1000);
        assert!(first.is_ok());
        let second = SqliteStore::open(&db_path, 1001);
        assert!(matches!(second, Err(StoreError::AlreadyLocked)));
    }
}

#[tokio::test]
async fn persists_job_repository_and_loads_admission_by_id() {
    let dir = tempdir().ok();
    assert!(dir.is_some());
    if let Some(dir) = dir {
        let db_path = dir.path().join("open-ci.db");
        let store = SqliteStore::open(&db_path, 1000).ok();
        assert!(store.is_some());
        if let Some(store) = store {
            let record = open_ci_core::AdmissionRecord {
                id: "open-ci-adm1".to_owned(),
                repository: "acme/service-a".to_owned(),
                run_id: 55,
                run_attempt: 1,
                head_sha: "a".repeat(40),
                pools: std::collections::BTreeSet::from(["api".to_owned()]),
                target: "open-ci".to_owned(),
                reason: open_ci_core::AdmissionReason::Admitted,
                status: open_ci_core::AdmissionStatus::Held,
                capacity: open_ci_core::Capacity {
                    vcpu: 4,
                    instances: 2,
                    disk_gb: 60,
                },
                runners: std::collections::BTreeMap::from([(
                    "api".to_owned(),
                    "open-ci-api".to_owned(),
                )]),
                created_at: 1000,
                released_at: None,
            };
            assert!(store.save_admission(&record).await.is_ok());
            let loaded = store.get_admission("open-ci-adm1").await.ok().flatten();
            assert_eq!(loaded, Some(record));

            assert!(
                store
                    .observe_job_hint(
                        900,
                        "api",
                        Some("acme/service-a"),
                        Some("open-ci-adm1"),
                        Some(55),
                        1010,
                    )
                    .await
                    .is_ok()
            );
            let jobs = store.list_jobs(Some("api")).await.unwrap_or_default();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].repository.as_deref(), Some("acme/service-a"));
            assert_eq!(jobs[0].admission_id.as_deref(), Some("open-ci-adm1"));
        }
        // Reopening after dropping the first store confirms migration 2 is idempotent.
        let reopened = SqliteStore::open(&db_path, 1020);
        assert!(reopened.is_ok());
    }
}

#[tokio::test]
async fn upsert_worker_and_job_enforce_monotonic_state_and_preserve_admission() {
    let dir = tempdir().ok();
    assert!(dir.is_some());
    if let Some(dir) = dir {
        let store = SqliteStore::open(&dir.path().join("open-ci.db"), 1000).ok();
        assert!(store.is_some());
        if let Some(store) = store {
            let creating = Worker {
                name: "open-ci-mono-1".to_owned(),
                pool_id: "default".to_owned(),
                admission_id: Some("adm-1".to_owned()),
                request_id: "req-mono-1".to_owned(),
                provider: ProviderKind::Mock,
                placement_index: 0,
                region: "local-1".to_owned(),
                zone: "local-1a".to_owned(),
                machine_type: "mock-standard-2".to_owned(),
                provider_instance_id: Some("mock-1".to_owned()),
                phase: WorkerPhase::Creating,
                create_operation: Some("op-1".to_owned()),
                delete_operation: None,
                delete_attempts: 0,
                bootstrap_used: false,
                runner_id: None,
                job_id: None,
                retire_reason: None,
                created_at: 1000,
                updated_at: 1000,
            };
            assert!(store.upsert_worker(&creating).await.is_ok());
            assert!(
                store
                    .consume_bootstrap("open-ci-mono-1", 1010)
                    .await
                    .is_ok()
            );

            // Stale Creating snapshot must not regress phase, restore create_operation, or clear bootstrap_used.
            assert!(store.upsert_worker(&creating).await.is_ok());
            let after_stale = store.get_worker("open-ci-mono-1").await.ok().flatten();
            assert!(matches!(
                after_stale,
                Some(ref w) if w.phase == WorkerPhase::Live && w.bootstrap_used && w.create_operation.is_none()
            ));

            // Advancing Retiring -> Gone clears runner_id when GitHub runner deletion succeeds.
            let retiring = Worker {
                phase: WorkerPhase::Retiring,
                runner_id: Some(77),
                updated_at: 1020,
                ..creating.clone()
            };
            assert!(store.upsert_worker(&retiring).await.is_ok());
            let gone = Worker {
                phase: WorkerPhase::Gone,
                runner_id: None,
                updated_at: 1030,
                ..retiring
            };
            assert!(store.upsert_worker(&gone).await.is_ok());
            let after_gone = store.get_worker("open-ci-mono-1").await.ok().flatten();
            assert!(matches!(
                after_gone,
                Some(ref w) if w.phase == WorkerPhase::Gone && w.runner_id.is_none()
            ));

            // Stale job upsert must not regress Completed -> Queued or clear admission_id/conclusion.
            let completed = open_ci_core::Job {
                id: 500,
                pool_id: "default".to_owned(),
                repository: Some("acme/repo".to_owned()),
                admission_id: Some("adm-1".to_owned()),
                run_id: Some(42),
                status: open_ci_core::JobStatus::Completed,
                conclusion: Some("success".to_owned()),
                runner_name: Some("open-ci-mono-1".to_owned()),
                runner_id: Some(77),
                checked_at: Some(1030),
                updated_at: 1030,
            };
            assert!(store.upsert_job(&completed).await.is_ok());
            let stale_job = open_ci_core::Job {
                id: 500,
                pool_id: "default".to_owned(),
                repository: None,
                admission_id: None,
                run_id: None,
                status: open_ci_core::JobStatus::Queued,
                conclusion: None,
                runner_name: None,
                runner_id: None,
                checked_at: None,
                updated_at: 1005,
            };
            assert!(store.upsert_job(&stale_job).await.is_ok());
            let jobs = store.list_jobs(Some("default")).await.unwrap_or_default();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].status, open_ci_core::JobStatus::Completed);
            assert_eq!(jobs[0].admission_id.as_deref(), Some("adm-1"));
            assert_eq!(jobs[0].conclusion.as_deref(), Some("success"));
        }
    }
}
