//! Loopback HTTP contract tests for `AwsProvider` (`SigV4`, Spot, On-Demand, error mapping).

use axum::{
    Router,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use open_ci_core::config::{AwsProviderConfig, Placement};
use open_ci_core::{ManualClock, ProviderKind, SecretString};
use open_ci_provider::{CloudProvider, CreateReceipt, InstanceSpec, ProviderError};
use open_ci_provider_aws::sigv4::AwsCredentials;
use open_ci_provider_aws::{AwsProvider, StaticAwsCredentials};
use reqwest::Client;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

fn test_placement(spot: bool) -> Placement {
    Placement {
        provider: ProviderKind::Aws,
        region: "us-east-1".to_owned(),
        zone: "us-east-1a".to_owned(),
        machine_type: "c7g.xlarge".to_owned(),
        subnet: "subnet-0123456789abcdef0".to_owned(),
        spot,
        disk_type: None,
        disk_interface: None,
        nic_type: None,
        image: Some("ami-0123456789abcdef0".to_owned()),
        security_group: Some("sg-0123456789abcdef0".to_owned()),
        root_device: Some("/dev/sda1".to_owned()),
        max_spot_price_usd: spot.then(|| "0.04".to_owned()),
    }
}

fn mock_aws_router() -> Router {
    Router::new().route(
        "/",
        post(|headers: HeaderMap, body: String| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIA_TEST/20231114/us-east-1/"));
            assert!(auth.contains("x-amz-content-sha256"));
            if body.contains("Action=GetCallerIdentity") {
                return (
                    StatusCode::OK,
                    "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>123456789012</Account></GetCallerIdentityResult></GetCallerIdentityResponse>".to_owned(),
                );
            }
            for (marker, code) in [
                ("pricecap-worker", "SpotMaxPriceTooLow"),
                ("unsupported-worker", "Unsupported"),
                ("slr-worker", "AuthFailure.ServiceLinkedRoleCreationNotPermitted"),
            ] {
                if body.contains("Action=RunInstances") && body.contains(marker) {
                    return (
                        StatusCode::BAD_REQUEST,
                        format!("<Response><Errors><Error><Code>{code}</Code></Error></Errors></Response>"),
                    );
                }
            }
            if body.contains("Action=RunInstances") && body.contains("stockout-worker") {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "<Response><Errors><Error><Code>InsufficientInstanceCapacity</Code></Error></Errors></Response>".to_owned(),
                );
            }
            if body.contains("Action=RunInstances") {
                assert!(body.contains("open-ci-managed"));
                assert!(body.contains("MetadataOptions.HttpTokens=required"));
                assert!(body.contains("BlockDeviceMapping.1.Ebs.Encrypted=true"));
                if body.contains("ondemand-worker") {
                    assert!(!body.contains("InstanceMarketOptions.MarketType=spot"));
                } else {
                    assert!(body.contains("InstanceMarketOptions.MarketType=spot"));
                }
                return (
                    StatusCode::OK,
                    "<RunInstancesResponse><instancesSet><item><instanceId>i-0abcdef1234567890</instanceId><instanceState><name>pending</name></instanceState><placement><availabilityZone>us-east-1a</availabilityZone></placement></item></instancesSet></RunInstancesResponse>".to_owned(),
                );
            }
            (StatusCode::BAD_REQUEST, "<Response/>".to_owned())
        }),
    )
}

async fn spawn_mock_aws() -> Option<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = listener.local_addr().ok()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, mock_aws_router()).await;
    });
    Some(addr)
}

fn test_provider(addr: SocketAddr) -> AwsProvider {
    AwsProvider::with_clock(
        AwsProviderConfig {
            account_id: "123456789012".to_owned(),
            endpoint_url: Some(format!("http://{addr}/")),
            role_arn: None,
            external_id: None,
            role_session_name: None,
            web_identity_audience: None,
        },
        Client::new(),
        Arc::new(StaticAwsCredentials(AwsCredentials {
            access_key_id: "AKIA_TEST".to_owned(),
            secret_access_key: SecretString::new("SECRET_TEST"),
            session_token: None,
        })),
        Arc::new(ManualClock::new(1_700_000_000)),
    )
}

fn instance_spec(worker_name: &str, placement: Placement) -> InstanceSpec {
    InstanceSpec {
        worker_name: worker_name.to_owned(),
        pool_id: "default".to_owned(),
        request_id: format!("req-{worker_name}"),
        placement,
        disk_gb: 30,
        lifetime_seconds: 1800,
        expires_at_unix: 1_700_001_800,
        user_data: "#!/bin/sh\n".to_owned(),
    }
}

#[tokio::test]
async fn native_aws_signs_requests_supports_spot_and_ondemand_and_maps_errors() {
    let addr = spawn_mock_aws().await;
    assert!(addr.is_some());
    let Some(addr) = addr else { return };
    let provider = test_provider(addr);
    let spot_placement = test_placement(true);
    assert!(provider.health_check(&spot_placement).await.is_ok());

    let receipt = provider
        .create_instance(&instance_spec("open-ci-default-1", spot_placement.clone()))
        .await;
    assert_eq!(
        receipt,
        Ok(CreateReceipt {
            provider_instance_id: Some("i-0abcdef1234567890".to_owned()),
            operation: None,
        })
    );

    let ondemand = provider
        .create_instance(&instance_spec("ondemand-worker", test_placement(false)))
        .await;
    assert!(ondemand.is_ok());

    let stockout = provider
        .create_instance(&instance_spec("stockout-worker", spot_placement))
        .await;
    assert_eq!(
        stockout,
        Err(ProviderError::CapacityUnavailable {
            zone: "us-east-1a".to_owned()
        })
    );
}

#[tokio::test]
async fn spot_price_and_unsupported_map_to_capacity_unavailable_and_missing_spot_role_fails_auth() {
    let addr = spawn_mock_aws().await;
    assert!(addr.is_some());
    let Some(addr) = addr else { return };
    let provider = test_provider(addr);
    let capacity = ProviderError::CapacityUnavailable {
        zone: "us-east-1a".to_owned(),
    };
    for (worker, expected) in [
        ("pricecap-worker", capacity.clone()),
        ("unsupported-worker", capacity),
        ("slr-worker", ProviderError::AuthFailed),
    ] {
        let result = provider
            .create_instance(&instance_spec(worker, test_placement(true)))
            .await;
        assert_eq!(result, Err(expected), "{worker}");
    }
}
