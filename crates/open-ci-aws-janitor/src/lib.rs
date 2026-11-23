//! Pure-Rust AWS expiry janitor for `open-ci`.
//!
//! Replaces the legacy Python Lambda with a fault-isolated Rust sweep:
//! a single failed `TerminateInstances` or `DeleteVolume` call NEVER aborts
//! cleanup of remaining expired resources.

use open_ci_provider_aws::AwsProvider;
use open_ci_provider_aws::xml::{ParsedEbsVolume, ParsedEc2Instance};
use serde::Serialize;
use std::collections::BTreeMap;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Structured report emitted after every janitor sweep.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub checked_at_unix: u64,
    pub region: String,
    pub pool: Option<String>,
    pub dry_run: bool,
    pub terminated_instances: Vec<String>,
    pub deleted_volumes: Vec<String>,
    pub failed_instances: BTreeMap<String, String>,
    pub failed_volumes: BTreeMap<String, String>,
}

impl SweepReport {
    pub fn has_failures(&self) -> bool {
        !self.failed_instances.is_empty() || !self.failed_volumes.is_empty()
    }
}

/// Evaluates whether a tagged AWS resource has exceeded its hard expiry deadline.
///
/// Missing or unparseable `open-ci-expires-at` tags on an owned resource fail closed
/// (treated as expired) so misconfigured instances cannot run indefinitely.
pub fn is_expired(
    tags: &BTreeMap<String, String>,
    created_rfc3339: Option<&str>,
    max_ceiling_seconds: u64,
    now_unix: u64,
) -> bool {
    if tags.get("open-ci-managed").map(String::as_str) != Some("true") {
        return false;
    }
    let Ok(mut deadline) = tags
        .get("open-ci-expires-at")
        .ok_or(())
        .and_then(|v| v.parse::<u64>().map_err(|_| ()))
    else {
        return true;
    };
    if let Some(raw_ts) = created_rfc3339
        && let Ok(dt) = OffsetDateTime::parse(raw_ts, &Rfc3339)
        && let Ok(launched) = u64::try_from(dt.unix_timestamp())
    {
        deadline = deadline.min(launched.saturating_add(max_ceiling_seconds));
    }
    now_unix >= deadline
}

fn instance_expired(inst: &ParsedEc2Instance, max_ceiling_seconds: u64, now_unix: u64) -> bool {
    if matches!(inst.state_name.as_str(), "terminated" | "shutting-down") {
        return false;
    }
    is_expired(
        &inst.tags,
        inst.launch_time_rfc3339.as_deref(),
        max_ceiling_seconds,
        now_unix,
    )
}

fn volume_expired(vol: &ParsedEbsVolume, max_ceiling_seconds: u64, now_unix: u64) -> bool {
    if vol.attached || vol.status != "available" {
        return false;
    }
    is_expired(
        &vol.tags,
        vol.create_time_rfc3339.as_deref(),
        max_ceiling_seconds,
        now_unix,
    )
}

/// Sweeps expired EC2 instances and detached EBS volumes with per-resource error isolation.
pub async fn run_sweep(
    provider: &AwsProvider,
    region: &str,
    pool: Option<&str>,
    max_ceiling_seconds: u64,
    now_unix: u64,
    dry_run: bool,
) -> Result<SweepReport, String> {
    let mut report = SweepReport {
        checked_at_unix: now_unix,
        region: region.to_owned(),
        pool: pool.map(str::to_owned),
        dry_run,
        ..SweepReport::default()
    };

    let instances = provider
        .describe_managed_instances(region, pool)
        .await
        .map_err(|e| e.to_string())?;

    for inst in instances {
        if !instance_expired(&inst, max_ceiling_seconds, now_unix) {
            continue;
        }
        if dry_run {
            report.terminated_instances.push(inst.instance_id);
            continue;
        }
        match provider
            .terminate_instance_by_id(region, &inst.instance_id)
            .await
        {
            Ok(()) => report.terminated_instances.push(inst.instance_id),
            Err(err) => {
                report
                    .failed_instances
                    .insert(inst.instance_id, err.to_string());
            }
        }
    }

    let volumes = provider
        .describe_managed_volumes(region, pool)
        .await
        .map_err(|e| e.to_string())?;

    for vol in volumes {
        if !volume_expired(&vol, max_ceiling_seconds, now_unix) {
            continue;
        }
        if dry_run {
            report.deleted_volumes.push(vol.volume_id);
            continue;
        }
        match provider.delete_volume_by_id(region, &vol.volume_id).await {
            Ok(()) => report.deleted_volumes.push(vol.volume_id),
            Err(err) => {
                report.failed_volumes.insert(vol.volume_id, err.to_string());
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, http::StatusCode, routing::post};
    use open_ci_core::config::AwsProviderConfig;
    use open_ci_core::{ManualClock, SecretString};
    use open_ci_provider_aws::StaticAwsCredentials;
    use open_ci_provider_aws::sigv4::AwsCredentials;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn single_terminate_failure_does_not_abort_remaining_instances_or_volumes() {
        let app = Router::new().route(
            "/",
            post(|body: String| async move {
                if body.contains("Action=DescribeInstances") {
                    return (
                        StatusCode::OK,
                        "<DescribeInstancesResponse><reservationSet><item><instancesSet>
                            <item>
                                <instanceId>i-fail-1</instanceId>
                                <instanceState><name>running</name></instanceState>
                                <tagSet>
                                    <item><key>open-ci-managed</key><value>true</value></item>
                                    <item><key>open-ci-expires-at</key><value>900</value></item>
                                </tagSet>
                            </item>
                            <item>
                                <instanceId>i-ok-2</instanceId>
                                <instanceState><name>running</name></instanceState>
                                <tagSet>
                                    <item><key>open-ci-managed</key><value>true</value></item>
                                    <item><key>open-ci-expires-at</key><value>950</value></item>
                                </tagSet>
                            </item>
                            <item>
                                <instanceId>i-fresh-3</instanceId>
                                <instanceState><name>running</name></instanceState>
                                <tagSet>
                                    <item><key>open-ci-managed</key><value>true</value></item>
                                    <item><key>open-ci-expires-at</key><value>5000</value></item>
                                </tagSet>
                            </item>
                        </instancesSet></item></reservationSet></DescribeInstancesResponse>"
                            .to_owned(),
                    );
                }
                if body.contains("Action=TerminateInstances") && body.contains("i-fail-1") {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "<Response><Errors><Error><Code>UnauthorizedOperation</Code></Error></Errors></Response>".to_owned(),
                    );
                }
                if body.contains("Action=TerminateInstances") {
                    return (StatusCode::OK, "<TerminateInstancesResponse/>".to_owned());
                }
                if body.contains("Action=DescribeVolumes") {
                    return (
                        StatusCode::OK,
                        "<DescribeVolumesResponse><volumeSet>
                            <item>
                                <volumeId>vol-expired-1</volumeId>
                                <status>available</status>
                                <tagSet>
                                    <item><key>open-ci-managed</key><value>true</value></item>
                                    <item><key>open-ci-expires-at</key><value>800</value></item>
                                </tagSet>
                            </item>
                        </volumeSet></DescribeVolumesResponse>"
                            .to_owned(),
                    );
                }
                if body.contains("Action=DeleteVolume") {
                    return (StatusCode::OK, "<DeleteVolumeResponse/>".to_owned());
                }
                (StatusCode::BAD_REQUEST, "<Response/>".to_owned())
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.ok();
        assert!(listener.is_some());
        if let Some(listener) = listener {
            let addr = listener.local_addr().ok();
            assert!(addr.is_some());
            if let Some(addr) = addr {
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                });
                let provider = build_test_provider(addr);
                let report = run_sweep(&provider, "us-west-2", None, 3600, 1000, false).await;
                assert!(report.is_ok());
                if let Ok(report) = report {
                    assert_eq!(report.terminated_instances, vec!["i-ok-2"]);
                    assert_eq!(report.deleted_volumes, vec!["vol-expired-1"]);
                    assert!(report.failed_instances.contains_key("i-fail-1"));
                    assert!(report.has_failures());
                }
            }
        }
    }

    fn build_test_provider(addr: std::net::SocketAddr) -> AwsProvider {
        AwsProvider::with_clock(
            AwsProviderConfig {
                account_id: "123456789012".to_owned(),
                endpoint_url: Some(format!("http://{addr}/")),
                role_arn: None,
                external_id: None,
                role_session_name: None,
            },
            reqwest::Client::new(),
            Arc::new(StaticAwsCredentials(AwsCredentials {
                access_key_id: "AKIA_TEST".to_owned(),
                secret_access_key: SecretString::new("SECRET_TEST"),
                session_token: None,
            })),
            Arc::new(ManualClock::new(1000)),
        )
    }
}
