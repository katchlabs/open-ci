//! Live operator CLI (`open-ci ctl`) communicating over the Operator HTTP API.
//!
//! Talks to `/api/v1/*` rather than opening the `SQLite` database directly so
//! operators can inspect and control a running `open-ci serve` instance without
//! colliding with its exclusive writer lockfile.

use crate::ModeArg;
use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use open_ci_core::ControlMode;
use reqwest::Client;

#[derive(Subcommand)]
pub enum CtlAction {
    /// Print current control mode, capacity envelope, workers, and active backoffs as JSON.
    Status,
    /// Transition the durable control mode (`active`, `fallback-only`, `draining`, `paused`).
    SetMode {
        mode: ModeArg,
        #[arg(long, default_value = "cli")]
        actor: String,
        #[arg(long, default_value = "manual")]
        reason: String,
    },
    /// Trigger one immediate reconciliation pass on the live control plane.
    Reconcile,
}

pub async fn run_ctl(
    client: &Client,
    base_url: &str,
    token: Option<&str>,
    action: CtlAction,
) -> Result<()> {
    let base = base_url.trim_end_matches('/');
    let mut req = match action {
        CtlAction::Status => client.get(format!("{base}/api/v1/overview")),
        CtlAction::SetMode {
            mode,
            actor,
            reason,
        } => {
            let control_mode: ControlMode = mode.into();
            client
                .post(format!("{base}/api/v1/control/mode"))
                .json(&serde_json::json!({
                    "mode": control_mode.as_str(),
                    "actor": actor,
                    "reason": reason,
                }))
        }
        CtlAction::Reconcile => client.post(format!("{base}/api/v1/reconcile")),
    };

    if let Some(bearer) = token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(bearer);
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("failed contacting operator API at {base}"))?;
    let status = resp.status();
    let body = resp.text().await.context("failed reading API response")?;

    if !status.is_success() {
        return Err(anyhow!("operator API returned HTTP {status}: {body}"));
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        println!("{body}");
    }
    Ok(())
}
