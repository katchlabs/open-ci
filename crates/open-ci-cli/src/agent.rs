//! Worker VM bootstrap agent (`open-ci agent bootstrap`).
//!
//! Replaces embedded 85-line Bash + inline Python startup scripts with a
//! strongly-typed, shell-injection-free Rust runner launcher.

use anyhow::{Context, Result, anyhow, ensure};
use open_ci_core::WorkerUsage;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

const BOOTSTRAP_MAX_ATTEMPTS: usize = 8;
const BOOTSTRAP_RETRY_DELAY: Duration = Duration::from_secs(2);
const NVME_WORK_DIR: &str = "/mnt/nvme/work";
const NVME_TMP_DIR: &str = "/mnt/nvme/tmp";

#[derive(Deserialize)]
struct AgentEnvelope {
    public_url: String,
    worker_name: String,
    bootstrap_token: String,
    usage_token: String,
    lifetime_seconds: u64,
    #[serde(default)]
    cache_url: Option<String>,
    #[serde(default)]
    cache_token: Option<String>,
}

/// Reads the JSON bootstrap envelope from stdin, fetches a one-time JIT config,
/// executes the official GitHub Actions ephemeral runner as a non-root user,
/// reports NIC bytes, and powers off the ephemeral VM on exit.
pub async fn run_bootstrap(runner_dir: &Path) -> Result<()> {
    scrub_cloud_init_user_data();

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("failed reading bootstrap envelope from stdin")?;
    let env: AgentEnvelope =
        serde_json::from_str(raw.trim()).context("invalid bootstrap envelope JSON")?;

    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("open-ci-agent/0.1")
        .build()?;

    let result = execute_runner_lifecycle(&client, &env, runner_dir).await;
    maybe_poweroff_ephemeral_vm().await;
    result
}

async fn execute_runner_lifecycle(
    client: &Client,
    env: &AgentEnvelope,
    runner_dir: &Path,
) -> Result<()> {
    let jit_config = fetch_jit_config(client, env).await?;
    let run_script = runner_dir.join("run.sh");
    let timeout = Duration::from_secs(env.lifetime_seconds);

    let mut cmd = build_runner_command(&run_script, runner_dir, jit_config.trim(), env);
    let mut child = cmd
        .kill_on_drop(true)
        .spawn()
        .context("failed spawning official github actions runner")?;

    let _ = tokio::time::timeout(timeout, child.wait()).await;

    if let Some(usage) = read_linux_nic_bytes() {
        let usage_url = format!("{}/v1/workers/{}/usage", env.public_url, env.worker_name);
        let _ = client
            .post(&usage_url)
            .bearer_auth(&env.usage_token)
            .json(&usage)
            .send()
            .await;
    }

    Ok(())
}

async fn fetch_jit_config(client: &Client, env: &AgentEnvelope) -> Result<String> {
    let boot_url = format!(
        "{}/v1/workers/{}/bootstrap",
        env.public_url.trim_end_matches('/'),
        env.worker_name
    );

    for attempt in 1..=BOOTSTRAP_MAX_ATTEMPTS {
        let resp = client
            .post(&boot_url)
            .bearer_auth(&env.bootstrap_token)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let jit = r.text().await.context("failed reading jit config")?;
                ensure!(!jit.trim().is_empty(), "empty jit config received");
                return Ok(jit);
            }
            Ok(r)
                if attempt < BOOTSTRAP_MAX_ATTEMPTS
                    && (r.status() == StatusCode::CONFLICT || r.status().is_server_error()) =>
            {
                tokio::time::sleep(BOOTSTRAP_RETRY_DELAY).await;
            }
            Ok(r) => {
                return Err(anyhow!(
                    "bootstrap rejected with HTTP {}",
                    r.status().as_u16()
                ));
            }
            Err(_) if attempt < BOOTSTRAP_MAX_ATTEMPTS => {
                tokio::time::sleep(BOOTSTRAP_RETRY_DELAY).await;
            }
            Err(err) => {
                return Err(err).context("bootstrap request failed");
            }
        }
    }

    Err(anyhow!(
        "bootstrap did not succeed after {BOOTSTRAP_MAX_ATTEMPTS} attempts"
    ))
}

fn build_runner_command(
    run_script: &Path,
    runner_dir: &Path,
    jit_config: &str,
    envelope: &AgentEnvelope,
) -> Command {
    let env_cache_url = std::env::var("OPEN_CI_CACHE_URL").ok();
    let injected_env = resolve_runner_env(
        envelope,
        env_cache_url.as_deref(),
        Path::new(NVME_WORK_DIR),
        Path::new(NVME_TMP_DIR),
    );

    let mut cmd = if is_running_as_root() {
        let runner_user =
            std::env::var("OPEN_CI_RUNNER_USER").unwrap_or_else(|_| "runner".to_owned());
        let mut c = Command::new("runuser");
        c.arg("-u")
            .arg(runner_user)
            .arg("--")
            .arg(run_script)
            .arg("--jitconfig")
            .arg(jit_config)
            .current_dir(runner_dir);
        c
    } else {
        let mut c = Command::new(run_script);
        c.arg("--jitconfig").arg(jit_config).current_dir(runner_dir);
        c
    };

    cmd.envs(injected_env);
    cmd
}

/// Computes performance, transparent Actions Cache v2, and local `NVMe` scratch environment
/// variables injected into the GitHub Actions runner process.
fn resolve_runner_env(
    envelope: &AgentEnvelope,
    fallback_cache_url: Option<&str>,
    nvme_work_dir: &Path,
    nvme_tmp_dir: &Path,
) -> Vec<(&'static str, String)> {
    let mut vars = vec![
        ("DOCKER_BUILDKIT", "1".to_owned()),
        ("COMPOSE_DOCKER_CLI_BUILD", "1".to_owned()),
    ];

    let effective_cache_url = envelope
        .cache_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| fallback_cache_url.map(str::trim).filter(|s| !s.is_empty()));

    if let Some(cache_url) = effective_cache_url {
        vars.push(("ACTIONS_CACHE_SERVICE_V2", "true".to_owned()));
        vars.push(("ACTIONS_RESULTS_URL", cache_url.to_owned()));
        if let Some(cache_token) = envelope
            .cache_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            vars.push(("OPEN_CI_CACHE_TOKEN", cache_token.to_owned()));
        }
    }

    if is_writable_dir(nvme_work_dir) {
        vars.push((
            "RUNNER_WORKDIR",
            nvme_work_dir.to_string_lossy().into_owned(),
        ));
        if is_writable_dir(nvme_tmp_dir) {
            vars.push(("TMPDIR", nvme_tmp_dir.to_string_lossy().into_owned()));
        }
    }

    vars
}

fn is_writable_dir(dir: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(dir) else {
        return false;
    };
    if !meta.is_dir() || meta.permissions().readonly() {
        return false;
    }
    let probe = dir.join(format!(".open-ci-write-probe-{}", std::process::id()));
    let created = OpenOptions::new().write(true).create_new(true).open(&probe);
    if created.is_ok() {
        let _ = std::fs::remove_file(&probe);
        true
    } else {
        false
    }
}

fn is_running_as_root() -> bool {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        return status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|rest| rest.split_whitespace().nth(1))
            .is_some_and(|euid| euid == "0");
    }
    false
}

fn scrub_cloud_init_user_data() {
    for path in [
        "/var/lib/cloud/instance/user-data.txt",
        "/var/lib/cloud/instance/user-data.txt.i",
    ] {
        let _ = std::fs::remove_file(path);
    }
}

async fn maybe_poweroff_ephemeral_vm() {
    if !is_running_as_root() || std::env::var_os("OPEN_CI_AGENT_NO_POWEROFF").is_some() {
        return;
    }
    let _ = Command::new("systemctl").arg("poweroff").status().await;
}

/// Sums RX/TX bytes across physical network interfaces on Linux (`/sys/class/net`).
fn read_linux_nic_bytes() -> Option<WorkerUsage> {
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    let mut rx = 0u64;
    let mut tx = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.join("device").exists() {
            continue;
        }
        if let Ok(r) = std::fs::read_to_string(path.join("statistics/rx_bytes"))
            && let Ok(val) = r.trim().parse::<u64>()
        {
            rx = rx.saturating_add(val);
        }
        if let Ok(t) = std::fs::read_to_string(path.join("statistics/tx_bytes"))
            && let Ok(val) = t.trim().parse::<u64>()
        {
            tx = tx.saturating_add(val);
        }
    }
    (rx > 0 && tx > 0).then_some(WorkerUsage {
        rx_bytes: rx,
        tx_bytes: tx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn sample_envelope(cache_url: Option<&str>, cache_token: Option<&str>) -> AgentEnvelope {
        AgentEnvelope {
            public_url: "https://ci.example.com".to_owned(),
            worker_name: "w-1".to_owned(),
            bootstrap_token: "boot-tok".to_owned(),
            usage_token: "usage-tok".to_owned(),
            lifetime_seconds: 1800,
            cache_url: cache_url.map(str::to_owned),
            cache_token: cache_token.map(str::to_owned),
        }
    }

    #[test]
    fn envelope_deserializes_with_and_without_cache_fields() -> Result<()> {
        let legacy = r#"{
            "public_url": "https://ci.example.com",
            "worker_name": "w-1",
            "bootstrap_token": "b",
            "usage_token": "u",
            "lifetime_seconds": 900
        }"#;
        let parsed_legacy: AgentEnvelope = serde_json::from_str(legacy)?;
        assert!(parsed_legacy.cache_url.is_none());
        assert!(parsed_legacy.cache_token.is_none());

        let with_cache = r#"{
            "public_url": "https://ci.example.com",
            "worker_name": "w-2",
            "bootstrap_token": "b",
            "usage_token": "u",
            "lifetime_seconds": 900,
            "cache_url": "http://10.0.0.5:8090",
            "cache_token": "scoped-token-xyz"
        }"#;
        let parsed_cache: AgentEnvelope = serde_json::from_str(with_cache)?;
        assert_eq!(
            parsed_cache.cache_url.as_deref(),
            Some("http://10.0.0.5:8090")
        );
        assert_eq!(
            parsed_cache.cache_token.as_deref(),
            Some("scoped-token-xyz")
        );
        Ok(())
    }

    #[test]
    fn resolve_env_always_enables_buildkit_and_injects_cache_from_envelope_or_fallback() {
        let missing = Path::new("/nonexistent/open-ci-nvme-work");
        let missing_tmp = Path::new("/nonexistent/open-ci-nvme-tmp");

        // No cache configured
        let env_none: HashMap<_, _> =
            resolve_runner_env(&sample_envelope(None, None), None, missing, missing_tmp)
                .into_iter()
                .collect();
        assert_eq!(
            env_none.get("DOCKER_BUILDKIT").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            env_none.get("COMPOSE_DOCKER_CLI_BUILD").map(String::as_str),
            Some("1")
        );
        assert!(!env_none.contains_key("ACTIONS_CACHE_SERVICE_V2"));
        assert!(!env_none.contains_key("RUNNER_WORKDIR"));

        // Envelope cache_url takes precedence over fallback env var
        let env_direct: HashMap<_, _> = resolve_runner_env(
            &sample_envelope(Some("http://10.0.0.9:8090"), Some("tok-123")),
            Some("http://10.0.0.2:8090"),
            missing,
            missing_tmp,
        )
        .into_iter()
        .collect();
        assert_eq!(
            env_direct
                .get("ACTIONS_CACHE_SERVICE_V2")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            env_direct.get("ACTIONS_RESULTS_URL").map(String::as_str),
            Some("http://10.0.0.9:8090")
        );
        assert_eq!(
            env_direct.get("OPEN_CI_CACHE_TOKEN").map(String::as_str),
            Some("tok-123")
        );

        // Fallback OPEN_CI_CACHE_URL used when envelope omits cache_url
        let env_fallback: HashMap<_, _> = resolve_runner_env(
            &sample_envelope(None, None),
            Some("http://10.0.0.2:8090"),
            missing,
            missing_tmp,
        )
        .into_iter()
        .collect();
        assert_eq!(
            env_fallback
                .get("ACTIONS_CACHE_SERVICE_V2")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            env_fallback.get("ACTIONS_RESULTS_URL").map(String::as_str),
            Some("http://10.0.0.2:8090")
        );
        assert!(!env_fallback.contains_key("OPEN_CI_CACHE_TOKEN"));
    }

    #[test]
    fn resolve_env_injects_nvme_workdir_and_tmpdir_when_writable() -> Result<()> {
        let base =
            std::env::temp_dir().join(format!("open-ci-agent-nvme-test-{}", std::process::id()));
        let work = base.join("work");
        let tmp = base.join("tmp");
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(&tmp)?;

        let vars: HashMap<_, _> =
            resolve_runner_env(&sample_envelope(None, None), None, &work, &tmp)
                .into_iter()
                .collect();
        assert_eq!(
            vars.get("RUNNER_WORKDIR").map(String::as_str),
            Some(work.to_string_lossy().as_ref())
        );
        assert_eq!(
            vars.get("TMPDIR").map(String::as_str),
            Some(tmp.to_string_lossy().as_ref())
        );

        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }
}
