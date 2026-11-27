//! Thin CLI entrypoint for `open-ci`.
//!
//! All domain logic, storage, provider adapters, and HTTP serving live in
//! dedicated library crates; this binary only parses flags and dispatches
//! to focused command modules.

mod agent;
mod commands;
mod runner_redirect;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use commands::cache::run_cache;
use commands::ctl::{CtlAction, run_ctl};
use commands::demo::build_demo_config;
use commands::doctor::{DoctorArgs, run_doctor};
use commands::init::{InitArgs, run_init};
use commands::serve::{
    build_control_plane, build_http_client, load_config, serve_with_optional_cache,
    serve_with_reconciler,
};
use open_ci_core::{Clock, ControlMode, SystemClock};
use open_ci_github::broker::{BrokerConfig, rotate_credentials};
use open_ci_store::SqliteStore;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "open-ci",
    version,
    about = "Open CI — multi-cloud ephemeral GitHub Actions runner control plane"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the unified control plane (webhook ingress, OIDC admission, reconciler, operator API).
    Serve {
        #[arg(long, default_value = "config/open-ci.example.toml")]
        config: PathBuf,
        /// Also run the built-in GitHub Actions v2 transparent cache server in this process.
        #[arg(long)]
        with_cache: bool,
    },
    /// Generate a validated `open-ci.toml` from an opinionated preset (`gcp-spot`, `aws-spot`, `multi-cloud`, `bare-metal`, `hybrid`).
    Init(InitArgs),
    /// Run offline preflight checks against `open-ci.toml`, secret files, storage paths, and host prerequisites.
    Doctor(DoctorArgs),
    /// Run the native GitHub Actions v2 transparent cache server (`LocalFs` `NVMe` or S3/R2).
    Cache {
        #[arg(long, default_value = "config/open-ci.example.toml")]
        config: PathBuf,
    },
    /// Start a zero-cloud local sandbox backed by `MockProvider` for evaluation and UI development.
    Demo {
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
    },
    /// Validate an `open-ci.toml` configuration offline and print capacity bounds as JSON.
    Validate {
        #[arg(long)]
        config: PathBuf,
    },
    /// Operator control commands against a live `open-ci` control plane over HTTP.
    Ctl {
        #[arg(long, default_value = "http://127.0.0.1:8080", env = "OPEN_CI_URL")]
        url: String,
        #[arg(long, env = "OPEN_CI_OPERATOR_TOKEN")]
        token: Option<String>,
        #[command(subcommand)]
        action: CtlAction,
    },
    /// Create a consistent online `SQLite` backup snapshot without stopping `open-ci`.
    Backup {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Rotate GitHub App installation and optional GCP metadata tokens atomically.
    RotateCredentials {
        #[arg(long)]
        broker_config: PathBuf,
    },
    /// Ephemeral worker VM agent commands (executed inside Packer-built runner images).
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ModeArg {
    Active,
    FallbackOnly,
    Draining,
    Paused,
}

impl From<ModeArg> for ControlMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Active => Self::Active,
            ModeArg::FallbackOnly => Self::FallbackOnly,
            ModeArg::Draining => Self::Draining,
            ModeArg::Paused => Self::Paused,
        }
    }
}

#[derive(Subcommand)]
enum AgentAction {
    /// Fetch one-time JIT config from `open-ci` and run the official ephemeral runner.
    Bootstrap {
        #[arg(long, default_value = "/opt/actions-runner")]
        runner_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Validate { config } => {
            let cfg = load_config(&config)?;
            let summary = serde_json::json!({
                "valid": true,
                "scope": cfg.github_scope().display_target(),
                "repository": cfg.repository,
                "organization": cfg.organization,
                "admission_mode": cfg.admission.mode,
                "cache_enabled": cfg.cache.is_some(),
                "capacity": cfg.admission.capacity,
                "background": cfg.admission.background,
                "pools": cfg.pools.keys().collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        Command::Serve { config, with_cache } => {
            let cfg = load_config(&config)?;
            serve_with_optional_cache(cfg, with_cache).await
        }
        Command::Init(args) => run_init(&args),
        Command::Doctor(args) => run_doctor(&args),
        Command::Cache { config } => {
            let cfg = load_config(&config)?;
            run_cache(&cfg).await
        }
        Command::Demo { listen } => {
            let cfg = build_demo_config(&listen);
            let cp = build_control_plane(cfg, true)?;
            tracing::info!(
                listen = %listen,
                operator_token = "demo-operator-token-000000000000",
                "starting open-ci local demo sandbox"
            );
            serve_with_reconciler(cp).await
        }
        Command::Ctl { url, token, action } => {
            let client = build_http_client(15)?;
            run_ctl(&client, &url, token.as_deref(), action).await
        }
        Command::Backup { config, output } => {
            let cfg = load_config(&config)?;
            SqliteStore::backup_file_online(
                PathBuf::from(cfg.server.database_path),
                output.clone(),
            )
            .await?;
            println!(
                "{}",
                serde_json::json!({ "status": "ok", "backup": output })
            );
            Ok(())
        }
        Command::RotateCredentials { broker_config } => {
            let raw = std::fs::read_to_string(&broker_config)
                .with_context(|| format!("failed reading {}", broker_config.display()))?;
            let cfg: BrokerConfig = toml::from_str(&raw)?;
            let client = build_http_client(15)?;
            rotate_credentials(&cfg, &client, SystemClock.now_unix()).await?;
            println!("open-ci credentials rotated.");
            Ok(())
        }
        Command::Agent {
            action: AgentAction::Bootstrap { runner_dir },
        } => agent::run_bootstrap(&runner_dir).await,
    }
}
