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
