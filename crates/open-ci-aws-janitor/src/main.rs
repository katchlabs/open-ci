//! CLI and AWS Lambda (`provided.al2023`) entrypoint for `open-ci-aws-janitor`.
//!
//! When `AWS_LAMBDA_RUNTIME_API` is present, enters the standard AWS Lambda
//! custom runtime HTTP loop; otherwise executes one sweep and exits.

use clap::Parser;
use open_ci_aws_janitor::{SweepReport, run_sweep};
use open_ci_core::config::AwsProviderConfig;
use open_ci_core::{Clock, SystemClock};
use open_ci_provider_aws::{AwsProvider, EnvAwsCredentials};
use reqwest::Client;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "open-ci-aws-janitor",
    about = "Independent AWS expiry guard for open-ci"
)]
struct Args {
    #[arg(long, env = "AWS_REGION")]
    region: String,
    #[arg(long, env = "OPEN_CI_AWS_ACCOUNT_ID", default_value = "000000000000")]
    account_id: String,
    #[arg(long, env = "OPEN_CI_POOL")]
    pool: Option<String>,
    #[arg(long, env = "JANITOR_MAX_CEILING_SECONDS", default_value_t = 3600)]
    max_ceiling_seconds: u64,
    #[arg(long, env = "JANITOR_DRY_RUN", default_value_t = false)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let Ok(http) = Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    else {
        tracing::error!("failed constructing HTTP client");
        return ExitCode::FAILURE;
    };

    let provider = AwsProvider::new(
        AwsProviderConfig {
            account_id: args.account_id.clone(),
            endpoint_url: None,
            role_arn: None,
            external_id: None,
            role_session_name: None,
        },
        http.clone(),
        Arc::new(EnvAwsCredentials),
    );

    if let Ok(runtime_api) = std::env::var("AWS_LAMBDA_RUNTIME_API")
        && !runtime_api.is_empty()
    {
        return run_lambda_loop(&http, &runtime_api, &provider, &args).await;
    }

    match execute_once(&provider, &args).await {
        Ok(report) => {
            if let Ok(json) = serde_json::to_string_pretty(&report) {
                println!("{json}");
            }
            if report.has_failures() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(err) => {
            tracing::error!(error = %err, "aws janitor sweep failed");
            ExitCode::FAILURE
        }
    }
}

async fn execute_once(provider: &AwsProvider, args: &Args) -> Result<SweepReport, String> {
    let now = SystemClock.now_unix();
    run_sweep(
        provider,
        &args.region,
        args.pool.as_deref(),
        args.max_ceiling_seconds,
        now,
        args.dry_run,
    )
    .await
    .map_err(|e| e.to_string())
}

async fn run_lambda_loop(
    http: &Client,
    runtime_api: &str,
    provider: &AwsProvider,
    args: &Args,
) -> ExitCode {
    let next_url = format!("http://{runtime_api}/2018-06-01/runtime/invocation/next");
    loop {
        let Ok(resp) = http.get(&next_url).send().await else {
            tracing::error!("failed polling AWS Lambda runtime invocation/next");
            return ExitCode::FAILURE;
        };
        let Some(req_id) = resp
            .headers()
            .get("lambda-runtime-aws-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        else {
            tracing::error!("missing lambda-runtime-aws-request-id header");
            return ExitCode::FAILURE;
        };
        let _ = resp.bytes().await;

        match execute_once(provider, args).await {
            Ok(report) if !report.has_failures() => {
                let url =
                    format!("http://{runtime_api}/2018-06-01/runtime/invocation/{req_id}/response");
                let _ = http.post(&url).json(&report).send().await;
            }
            Ok(report) => {
                let url =
                    format!("http://{runtime_api}/2018-06-01/runtime/invocation/{req_id}/error");
                let total_failures = report.failed_instances.len() + report.failed_volumes.len();
                let body = serde_json::json!({
                    "errorType": "JanitorPartialFailure",
                    "errorMessage": format!("{total_failures} janitor cleanup failures"),
                });
                let _ = http.post(&url).json(&body).send().await;
            }
            Err(err) => {
                let url =
                    format!("http://{runtime_api}/2018-06-01/runtime/invocation/{req_id}/error");
                let body = serde_json::json!({
                    "errorType": "JanitorSweepError",
                    "errorMessage": err,
                });
                let _ = http.post(&url).json(&body).send().await;
            }
        }
    }
}
