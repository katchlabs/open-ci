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
