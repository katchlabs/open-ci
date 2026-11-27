//! Loopback HTTP contract tests for `GcpProvider`.

use axum::{
    Json, Router,
    routing::{get, post},
};
use open_ci_core::config::{GcpProviderConfig, Placement};
use open_ci_core::{ProviderKind, SecretString};
use open_ci_provider::{CloudProvider, InstanceSpec, OperationRef, OperationStatus, ProviderError};
use open_ci_provider_gcp::{GcpProvider, StaticTokenSource};
use reqwest::Client;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

fn test_placement(spot: bool) -> Placement {
    Placement {
        provider: ProviderKind::Gcp,
        region: "us-central1".to_owned(),
        zone: "us-central1-a".to_owned(),
        machine_type: "n2d-standard-4".to_owned(),
        subnet: "projects/demo/regions/us-central1/subnetworks/ci".to_owned(),
        spot,
        disk_type: None,
        disk_interface: None,
        nic_type: None,
        image: None,
        security_group: None,
        root_device: None,
        max_spot_price_usd: None,
        core_count: None,
        threads_per_core: None,
        local_ssd_count: None,
        network_bandwidth_tier: None,
        ebs_iops: None,
        ebs_throughput_mbps: None,
        placement_group: None,
    }
}

fn mock_gcp_router() -> Router {
    Router::new()
        .route(
            "/projects/demo/zones/us-central1-a/instances",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["serviceAccounts"], json!([]));
                assert_eq!(body["labels"]["open-ci-managed"], json!("true"));
                assert_eq!(
                    body["shieldedInstanceConfig"]["enableSecureBoot"],
                    json!(true)
                );
                assert_eq!(
                    body["metadata"]["items"][0]["key"],
                    json!("block-project-ssh-keys")
                );
                let expected = if body["name"] == json!("open-ci-ondemand-1") {
                    "STANDARD"
                } else {
                    "SPOT"
                };
                assert_eq!(body["scheduling"]["provisioningModel"], json!(expected));
                Json(json!({ "name": "op-1", "status": "RUNNING" }))
            }),
        )
        .route(
            "/projects/demo/zones/us-central1-a/operations/op-1",
            get(|| async {
                Json(json!({
                    "name": "op-1",
                    "status": "DONE",
                    "error": {
                        "errors": [{
                            "code": "QUOTA_EXCEEDED",
                            "errorDetails": [{
                                "quotaInfo": {
                                    "dimensions": { "region": "us-central1", "vm_family": "n2d" }
                                }
                            }]
                        }]
                    }
                }))
            }),
        )
}

#[tokio::test]
async fn gcp_adapter_supports_spot_and_ondemand_and_detects_quota_errors() {
    let app = mock_gcp_router();

    let listener = TcpListener::bind("127.0.0.1:0").await.ok();
    assert!(listener.is_some());
    if let Some(listener) = listener {
        let addr = listener.local_addr().ok();
        assert!(addr.is_some());
        if let Some(addr) = addr {
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let provider = GcpProvider::new(
                GcpProviderConfig {
                    project: "demo".to_owned(),
                    compute_url: format!("http://{addr}"),
                    image: "projects/demo/global/images/runner-v1".to_owned(),
                    external_ipv4: false,
                    network_tier: "PREMIUM".to_owned(),
                    network_tags: vec!["open-ci-worker".to_owned()],
                    impersonate_service_account: None,
                },
                Client::new(),
                Arc::new(StaticTokenSource(SecretString::new("tok"))),
            );

            let spot_placement = test_placement(true);
            let receipt = provider
                .create_instance(&InstanceSpec {
                    worker_name: "open-ci-default-1".to_owned(),
                    pool_id: "default".to_owned(),
                    request_id: "req-1".to_owned(),
                    placement: spot_placement.clone(),
                    disk_gb: 40,
                    lifetime_seconds: 1800,
                    expires_at_unix: 2000,
                    user_data: "#!/bin/sh\n".to_owned(),
                    tenant_id: None,
                })
                .await;
            assert!(receipt.is_ok());

            let ondemand_receipt = provider
                .create_instance(&InstanceSpec {
                    worker_name: "open-ci-ondemand-1".to_owned(),
                    pool_id: "default".to_owned(),
                    request_id: "req-2".to_owned(),
                    placement: test_placement(false),
                    disk_gb: 40,
                    lifetime_seconds: 1800,
                    expires_at_unix: 2000,
                    user_data: "#!/bin/sh\n".to_owned(),
                    tenant_id: None,
                })
                .await;
            assert!(ondemand_receipt.is_ok());

            if let Ok(receipt) = receipt {
                let op_ref = receipt
                    .operation
                    .unwrap_or_else(|| OperationRef(String::new()));
                let status = provider.poll_operation(&spot_placement, &op_ref).await;
                assert_eq!(
                    status,
                    Ok(OperationStatus::Failed(ProviderError::QuotaExceeded {
                        region: "us-central1".to_owned(),
                        family: "n2d".to_owned(),
                    }))
                );
            }
        }
    }
}

fn instance_spec(worker_name: &str, request_id: &str, placement: Placement) -> InstanceSpec {
    InstanceSpec {
        worker_name: worker_name.to_owned(),
        pool_id: "default".to_owned(),
        request_id: request_id.to_owned(),
        placement,
        disk_gb: 40,
        lifetime_seconds: 1800,
        expires_at_unix: 2000,
        user_data: "#!/bin/sh\n".to_owned(),
        tenant_id: None,
    }
}

#[tokio::test]
async fn gcp_insert_wires_threads_per_core_local_ssds_and_network_bandwidth_tier() {
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let capture_clone = Arc::clone(&captured);
    let app = Router::new().route(
        "/projects/demo/zones/us-central1-a/instances",
        post(move |Json(body): Json<Value>| {
            let capture_clone = Arc::clone(&capture_clone);
            async move {
                capture_clone.lock().await.push(body);
                Json(json!({ "name": "op-hw", "status": "RUNNING" }))
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.ok();
    assert!(listener.is_some());
    let Some(listener) = listener else { return };
    let addr = listener.local_addr().ok();
    assert!(addr.is_some());
    let Some(addr) = addr else { return };
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let provider = GcpProvider::new(
        GcpProviderConfig {
            project: "demo".to_owned(),
            compute_url: format!("http://{addr}"),
            image: "projects/demo/global/images/runner-v1".to_owned(),
            external_ipv4: false,
            network_tier: "PREMIUM".to_owned(),
            network_tags: vec!["open-ci-worker".to_owned()],
            impersonate_service_account: None,
        },
        Client::new(),
        Arc::new(StaticTokenSource(SecretString::new("tok"))),
    );

    assert!(
        provider
            .create_instance(&instance_spec(
                "open-ci-default-hw",
                "req-hw-1",
                test_placement(true)
            ))
            .await
            .is_ok()
    );

    let mut tuned = test_placement(true);
    tuned.threads_per_core = Some(1);
    tuned.local_ssd_count = Some(2);
    tuned.network_bandwidth_tier = Some("TIER_1".to_owned());

    assert!(
        provider
            .create_instance(&instance_spec("open-ci-tuned-hw", "req-hw-2", tuned))
            .await
            .is_ok()
    );

    let bodies = captured.lock().await;
    assert_eq!(bodies.len(), 2);
    if let [default_body, tuned_body] = bodies.as_slice() {
        assert_eq!(default_body["disks"].as_array().map(Vec::len), Some(1));
        assert!(default_body.get("advancedMachineFeatures").is_none());
        assert!(default_body.get("networkPerformanceConfig").is_none());

        assert_eq!(tuned_body["serviceAccounts"], json!([]));
        assert_eq!(
            tuned_body["advancedMachineFeatures"],
            json!({ "threadsPerCore": 1 })
        );
        assert_eq!(
            tuned_body["networkPerformanceConfig"],
            json!({ "totalEgressBandwidthTier": "TIER_1" })
        );
        assert_eq!(tuned_body["disks"].as_array().map(Vec::len), Some(3));
        let expected_ssd = json!({
            "type": "SCRATCH",
            "autoDelete": true,
            "interface": "NVME",
            "initializeParams": {
                "diskType": "zones/us-central1-a/diskTypes/local-ssd"
            }
        });
        assert_eq!(tuned_body["disks"][1], expected_ssd);
        assert_eq!(tuned_body["disks"][2], expected_ssd);
    }
}

fn build_paginated_gcp_router(captured_insert: Arc<Mutex<Vec<Value>>>) -> Router {
    use axum::extract::RawQuery;

    Router::new()
        .route(
            "/projects/demo/zones/us-central1-a/instances",
            post(move |Json(body): Json<Value>| {
                let capture = Arc::clone(&captured_insert);
                async move {
                    capture.lock().await.push(body);
                    Json(json!({ "name": "op-tenant", "status": "RUNNING" }))
                }
            })
            .get(|RawQuery(query): RawQuery| async move {
                let q = query.unwrap_or_default();
                let (next_page, id, name) = if q.contains("pageToken=p2") {
                    (None, "2", "open-ci-default-2")
                } else {
                    (Some("p2"), "1", "open-ci-default-1")
                };
                let mut payload = json!({
                    "items": [{
                        "id": id,
                        "name": name,
                        "status": "RUNNING",
                        "creationTimestamp": "2026-11-27T12:00:00.000-07:00",
                        "labels": {
                            "open-ci-managed": "true",
                            "open-ci-pool": "default",
                            "open-ci-tenant": "tenant_acme",
                            "open-ci-expires-at": "2000"
                        }
                    }]
                });
                if let Some(tok) = next_page {
                    payload["nextPageToken"] = json!(tok);
                }
                Json(payload)
            }),
        )
        .route(
            "/projects/demo/zones/us-central1-a/disks",
            get(|RawQuery(query): RawQuery| async move {
                let q = query.unwrap_or_default();
                let (next_page, id, name, users) = if q.contains("pageToken=d2") {
                    (None, "d-2", "open-ci-default-2", json!([]))
                } else {
                    (
                        Some("d2"),
                        "d-1",
                        "open-ci-default-1",
                        json!(["projects/demo/zones/us-central1-a/instances/open-ci-default-1"]),
                    )
                };
                let mut payload = json!({
                    "items": [{
                        "id": id,
                        "name": name,
                        "status": "READY",
                        "labels": {
                            "open-ci-managed": "true",
                            "open-ci-pool": "default",
                            "open-ci-tenant": "tenant_acme"
                        },
                        "users": users
                    }]
                });
                if let Some(tok) = next_page {
                    payload["nextPageToken"] = json!(tok);
                }
                Json(payload)
            }),
        )
}

#[tokio::test]
async fn gcp_paginates_managed_resources_and_sets_expiry_and_tenant_labels() {
    let captured_insert = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = build_paginated_gcp_router(Arc::clone(&captured_insert));

    let listener = TcpListener::bind("127.0.0.1:0").await.ok();
    assert!(listener.is_some());
    let Some(listener) = listener else { return };
    let addr = listener.local_addr().ok();
    assert!(addr.is_some());
    let Some(addr) = addr else { return };
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let provider = GcpProvider::new(
        GcpProviderConfig {
            project: "demo".to_owned(),
            compute_url: format!("http://{addr}"),
            image: "projects/demo/global/images/runner-v1".to_owned(),
            external_ipv4: false,
            network_tier: "PREMIUM".to_owned(),
            network_tags: vec!["open-ci-worker".to_owned()],
            impersonate_service_account: None,
        },
        Client::new(),
        Arc::new(StaticTokenSource(SecretString::new("tok"))),
    );

    let mut spec = instance_spec("open-ci-default-1", "req-t-1", test_placement(true));
    spec.tenant_id = Some("tenant_acme".to_owned());
    assert!(provider.create_instance(&spec).await.is_ok());

    let bodies = captured_insert.lock().await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["labels"]["open-ci-expires-at"], json!("2000"));
    assert_eq!(bodies[0]["labels"]["open-ci-tenant"], json!("tenant_acme"));
    assert_eq!(
        bodies[0]["disks"][0]["initializeParams"]["labels"]["open-ci-expires-at"],
        json!("2000")
    );
    assert_eq!(
        bodies[0]["disks"][0]["initializeParams"]["labels"]["open-ci-tenant"],
        json!("tenant_acme")
    );

    let instances = provider
        .list_managed_instances("us-central1-a", Some("default"), Some("tenant_acme"))
        .await;
    assert!(matches!(
        instances,
        Ok(ref list) if list.len() == 2 && list[0].id == "1" && list[1].id == "2"
    ));

    let disks = provider
        .list_managed_disks("us-central1-a", Some("default"), Some("tenant_acme"))
        .await;
    assert!(matches!(
        disks,
        Ok(ref list) if list.len() == 2 && list[0].attached && !list[1].attached
    ));

    let mut bad_spec = instance_spec("open-ci-bad", "req-bad", test_placement(true));
    bad_spec.tenant_id = Some("BAD TENANT!".to_owned());
    assert!(matches!(
        provider.create_instance(&bad_spec).await,
        Err(ProviderError::InvalidConfig(_))
    ));
}
