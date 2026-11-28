//! `open-ci doctor` — single-command preflight check for config, secrets, storage, and host readiness.

use anyhow::{Result, bail};
use clap::Args;
use open_ci_core::OpenCiConfig;
use open_ci_core::config::CacheStorageConfig;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Args)]
pub struct DoctorArgs {
    /// Path to `open-ci.toml` to verify.
    #[arg(long, default_value = "config/open-ci.example.toml")]
    pub config: PathBuf,
    /// Also verify requirements for running the embedded cache (`--with-cache`).
    #[arg(long)]
    pub with_cache: bool,
    /// Emit machine-readable JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckItem {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<CheckItem>,
}

pub fn run_doctor(args: &DoctorArgs) -> Result<()> {
    let report = evaluate_doctor(&args.config, args.with_cache, |var| std::env::var(var).ok());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for item in &report.checks {
            let tag = match item.status {
                CheckStatus::Pass => "PASS",
                CheckStatus::Warn => "WARN",
                CheckStatus::Fail => "FAIL",
            };
            println!("[{tag}] {}: {}", item.name, item.detail);
        }
    }
    if !report.ok {
        bail!("preflight doctor found one or more failing checks");
    }
    Ok(())
}

/// Runs all offline preflight checks using an injectable environment lookup for pure testing.
pub fn evaluate_doctor<F>(config_path: &Path, with_cache: bool, env_var: F) -> DoctorReport
where
    F: Fn(&str) -> Option<String>,
{
    let mut checks = Vec::new();
    let cfg = match std::fs::read_to_string(config_path) {
        Ok(raw) => match OpenCiConfig::parse_toml(&raw) {
            Ok(parsed) => {
                checks.push(CheckItem {
                    name: "config".to_owned(),
                    status: CheckStatus::Pass,
                    detail: format!(
                        "{} valid (scope={}, pools={})",
                        config_path.display(),
                        parsed.github_scope().display_target(),
                        parsed.pools.len()
                    ),
                });
                Some(parsed)
            }
            Err(err) => {
                checks.push(CheckItem {
                    name: "config".to_owned(),
                    status: CheckStatus::Fail,
                    detail: format!("invalid config {}: {err}", config_path.display()),
                });
                None
            }
        },
        Err(err) => {
            checks.push(CheckItem {
                name: "config".to_owned(),
                status: CheckStatus::Fail,
                detail: format!("cannot read {}: {err}", config_path.display()),
            });
            None
        }
    };

    check_secret_env(&mut checks, &env_var, "OPEN_CI_GITHUB_TOKEN_FILE", true, 1);
    check_secret_env(
        &mut checks,
        &env_var,
        "OPEN_CI_WEBHOOK_SECRET_FILE",
        true,
        32,
    );
    check_secret_env(
        &mut checks,
        &env_var,
        "OPEN_CI_BOOTSTRAP_SECRET_FILE",
        true,
        32,
    );
    check_secret_env(
        &mut checks,
        &env_var,
        "OPEN_CI_OPERATOR_TOKEN_FILE",
        false,
        24,
    );

    if let Some(cfg) = &cfg {
        check_parent_dir(
            &mut checks,
            "sqlite_dir",
            Path::new(&cfg.server.database_path),
        );
        if with_cache && cfg.cache.is_none() {
            checks.push(CheckItem {
                name: "cache_config".to_owned(),
                status: CheckStatus::Fail,
                detail: "`--with-cache` requires a `[cache]` section in open-ci.toml".to_owned(),
            });
        }
        if let Some(cache) = &cfg.cache {
            check_secret_env(
                &mut checks,
                &env_var,
                "OPEN_CI_CACHE_SIGNING_KEY_FILE",
                with_cache,
                32,
            );
            check_parent_dir(&mut checks, "cache_index_dir", Path::new(&cache.index_path));
            if let CacheStorageConfig::Local { path } = &cache.storage {
                check_parent_dir(&mut checks, "cache_storage_dir", Path::new(path));
            }
        }
        if let Some(ch) = &cfg.cloud_hypervisor {
            check_cloud_hypervisor_host(&mut checks, ch);
        }
    }

    let ok = checks.iter().all(|c| c.status != CheckStatus::Fail);
    DoctorReport { ok, checks }
}

fn check_cloud_hypervisor_host(
    checks: &mut Vec<CheckItem>,
    ch: &open_ci_core::config::CloudHypervisorProviderConfig,
) {
    check_file_exists(checks, "ch_binary", Path::new(&ch.binary));
    check_file_exists(checks, "ch_kernel", Path::new(&ch.kernel_path));
    check_file_exists(checks, "ch_rootfs", Path::new(&ch.rootfs_image));
    check_parent_dir(checks, "ch_state_dir", Path::new(&ch.state_dir));
    #[cfg(target_os = "linux")]
    {
        let kvm = Path::new("/dev/kvm");
        if kvm.exists() {
            checks.push(CheckItem {
                name: "ch_kvm".to_owned(),
                status: CheckStatus::Pass,
                detail: "/dev/kvm present".to_owned(),
            });
        } else {
            checks.push(CheckItem {
                name: "ch_kvm".to_owned(),
                status: CheckStatus::Fail,
                detail: "/dev/kvm missing (enable nested virtualization or run on bare metal)"
                    .to_owned(),
            });
        }
    }
}

fn check_secret_env<F>(
    checks: &mut Vec<CheckItem>,
    env_var: &F,
    name: &str,
    required: bool,
    min_bytes: usize,
) where
    F: Fn(&str) -> Option<String>,
{
    let Some(path_str) = env_var(name).filter(|s| !s.trim().is_empty()) else {
        checks.push(CheckItem {
            name: name.to_owned(),
            status: if required {
                CheckStatus::Fail
            } else {
                CheckStatus::Warn
            },
            detail: if required {
                format!("unset; export {name}=/etc/open-ci/...")
            } else {
                "unset (optional)".to_owned()
            },
        });
        return;
    };

    let path = Path::new(&path_str);
    let raw = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            checks.push(CheckItem {
                name: name.to_owned(),
                status: CheckStatus::Fail,
                detail: format!("unreadable {}: {err}", path.display()),
            });
            return;
        }
    };
    let len = raw.trim().len();
    if len < min_bytes {
        checks.push(CheckItem {
            name: name.to_owned(),
            status: CheckStatus::Fail,
            detail: format!("{} is {len} bytes (minimum {min_bytes})", path.display()),
        });
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o007 != 0 {
                checks.push(CheckItem {
                    name: name.to_owned(),
                    status: CheckStatus::Warn,
                    detail: format!(
                        "{} is world-accessible (mode {mode:04o}; run `chmod 0600 {}`)",
                        path.display(),
                        path.display()
                    ),
                });
                return;
            }
        }
    }

    checks.push(CheckItem {
        name: name.to_owned(),
        status: CheckStatus::Pass,
        detail: format!("{} readable ({len} bytes)", path.display()),
    });
}

fn check_parent_dir(checks: &mut Vec<CheckItem>, name: &str, path: &Path) {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let Some(parent) = dir else {
        checks.push(CheckItem {
            name: name.to_owned(),
            status: CheckStatus::Pass,
            detail: format!("relative path {}", path.display()),
        });
        return;
    };
    if parent.is_dir() {
        checks.push(CheckItem {
            name: name.to_owned(),
            status: CheckStatus::Pass,
            detail: format!("{} exists", parent.display()),
        });
    } else {
        checks.push(CheckItem {
            name: name.to_owned(),
            status: CheckStatus::Warn,
            detail: format!(
                "{} does not exist yet (create before starting service)",
                parent.display()
            ),
        });
    }
}

fn check_file_exists(checks: &mut Vec<CheckItem>, name: &str, path: &Path) {
    if path.is_file() {
        checks.push(CheckItem {
            name: name.to_owned(),
            status: CheckStatus::Pass,
            detail: format!("{} present", path.display()),
        });
    } else {
        checks.push(CheckItem {
            name: name.to_owned(),
            status: CheckStatus::Fail,
            detail: format!("{} missing", path.display()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::init::{InitArgs, InitPreset, render_init_config};
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    #[test]
    fn doctor_passes_with_valid_config_and_secrets_and_flags_missing_files() {
        let dir = tempdir().ok();
        assert!(dir.is_some());
        let Some(dir) = dir else { return };

        let cfg_path = dir.path().join("open-ci.toml");
        let args = InitArgs {
            preset: InitPreset::GcpSpot,
            repo: Some("acme/app".to_owned()),
            org: None,
            public_url: "https://ci.acme.dev".to_owned(),
            gcp_project: "acme-ci".to_owned(),
            aws_account: "123456789012".to_owned(),
            region: None,
            zones: Vec::new(),
            arm64: false,
            with_cache: true,
            cache_url: None,
            max_instances: 4,
            min_idle: 0,
            local_max_instances: 4,
            output: cfg_path.clone(),
            force: true,
        };
        let rendered = render_init_config(&args);
        assert!(rendered.is_ok());
        if let Ok(text) = rendered {
            assert!(std::fs::write(&cfg_path, text).is_ok());
        }

        // Missing env vars must fail.
        let missing = evaluate_doctor(&cfg_path, true, |_| None);
        assert!(!missing.ok);

        // Populate private secret files and verify doctor passes cleanly.
        let mut env_map = BTreeMap::new();
        for (var, val) in [
            ("OPEN_CI_GITHUB_TOKEN_FILE", "ghs_valid_token"),
            ("OPEN_CI_WEBHOOK_SECRET_FILE", &"a".repeat(32)),
            ("OPEN_CI_BOOTSTRAP_SECRET_FILE", &"b".repeat(32)),
            ("OPEN_CI_OPERATOR_TOKEN_FILE", &"c".repeat(24)),
            ("OPEN_CI_CACHE_SIGNING_KEY_FILE", &"d".repeat(32)),
        ] {
            let path = dir.path().join(var);
            assert!(std::fs::write(&path, val).is_ok());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            env_map.insert(var.to_owned(), path.display().to_string());
        }

        let healthy = evaluate_doctor(&cfg_path, true, |k| env_map.get(k).cloned());
        assert!(
            healthy.ok,
            "unexpected doctor failure: {:?}",
            healthy.checks
        );
    }
}
