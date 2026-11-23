//! Typed GitHub REST API client with bounded pagination and cached async token loading.

use async_trait::async_trait;
use open_ci_core::{Architecture, GitHubScope, JobStatus, SecretString};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GitHubError {
    #[error("github authentication failed")]
    AuthFailed,

    #[error("github resource not found")]
    NotFound,

    #[error("github rate limit exceeded")]
    RateLimited,

    #[error("github request timed out")]
    Timeout,

    #[error("github api error: {0}")]
    Api(String),
}

#[async_trait]
pub trait GitHubTokenSource: Send + Sync + std::fmt::Debug {
    async fn bearer_token(&self) -> Result<SecretString, GitHubError>;
}

/// Asynchronously reads a rotated token file and caches it in memory for `ttl`.
#[derive(Debug)]
pub struct FileGitHubToken {
    path: PathBuf,
    ttl: Duration,
    cached: RwLock<Option<(SecretString, Instant)>>,
}

impl FileGitHubToken {
    pub fn new(path: PathBuf, ttl: Duration) -> Self {
        Self {
            path,
            ttl,
            cached: RwLock::new(None),
        }
    }
}

#[async_trait]
impl GitHubTokenSource for FileGitHubToken {
    async fn bearer_token(&self) -> Result<SecretString, GitHubError> {
        {
            let guard = self.cached.read().await;
            if let Some((token, at)) = guard.as_ref()
                && at.elapsed() < self.ttl
            {
                return Ok(token.clone());
            }
        }
        let raw = tokio::fs::read_to_string(&self.path)
            .await
            .map_err(|_| GitHubError::AuthFailed)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(GitHubError::AuthFailed);
        }
        let token = SecretString::new(trimmed);
        *self.cached.write().await = Some((token.clone(), Instant::now()));
        Ok(token)
    }
}

#[derive(Debug)]
pub struct StaticGitHubToken(pub SecretString);

#[async_trait]
impl GitHubTokenSource for StaticGitHubToken {
    async fn bearer_token(&self) -> Result<SecretString, GitHubError> {
        Ok(self.0.clone())
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GitHubJob {
    pub id: u64,
    pub run_id: u64,
    pub status: String,
    pub conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub runner_id: Option<u64>,
    #[serde(default)]
    pub labels: Vec<String>,
}

impl GitHubJob {
    pub fn parsed_status(&self) -> JobStatus {
        JobStatus::parse(&self.status)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GitHubRunner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GitHubWorkflowRun {
    pub id: u64,
    pub run_attempt: u32,
    pub status: String,
    pub conclusion: Option<String>,
}

#[derive(Deserialize)]
struct RunnersPage {
    #[serde(default)]
    runners: Vec<GitHubRunner>,
}

#[derive(Deserialize)]
struct JobsPage {
    #[serde(default)]
    jobs: Vec<GitHubJob>,
}

#[derive(Deserialize)]
struct JitConfigResponse {
    encoded_jit_config: String,
}

#[derive(Clone, Debug)]
pub struct GitHubClient {
    base_url: String,
    scope: GitHubScope,
    client: Client,
    tokens: Arc<dyn GitHubTokenSource>,
}

impl GitHubClient {
    pub fn new(
        base_url: String,
        scope: GitHubScope,
        client: Client,
        tokens: Arc<dyn GitHubTokenSource>,
    ) -> Self {
        Self {
            base_url,
            scope,
            client,
            tokens,
        }
    }

    pub fn scope(&self) -> &GitHubScope {
        &self.scope
    }

    fn runners_prefix(&self) -> String {
        match &self.scope {
            GitHubScope::Repository(repo) => format!("/repos/{repo}/actions/runners"),
            GitHubScope::Organization { org, .. } => format!("/orgs/{org}/actions/runners"),
        }
    }

    fn resolve_repo<'a>(&'a self, repo: Option<&'a str>) -> Result<&'a str, GitHubError> {
        let target = match repo {
            Some(r) => r,
            None => self.scope.default_repository().ok_or_else(|| {
                GitHubError::Api("repository required for org-scoped request".to_owned())
            })?,
        };
        if !self.scope.allows_repository(target) {
            return Err(GitHubError::Api(format!(
                "repository `{target}` is not allowed by configured scope"
            )));
        }
        Ok(target)
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        full_path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Option<T>, GitHubError> {
        let token = self.tokens.bearer_token().await?;
        let url = format!("{}{full_path}", self.base_url);
        let mut req = self
            .client
            .request(method, url)
            .bearer_auth(token.expose())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(payload) = body {
            req = req.json(&payload);
        }
        let resp = req.send().await.map_err(|err| {
            if err.is_timeout() {
                GitHubError::Timeout
            } else {
                GitHubError::Api("github transport error".to_owned())
            }
        })?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(GitHubError::AuthFailed);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(GitHubError::RateLimited);
        }
        if !status.is_success() {
            return Err(GitHubError::Api(format!("http {}", status.as_u16())));
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let parsed = resp
            .json::<T>()
            .await
            .map_err(|_| GitHubError::Api("invalid github json".to_owned()))?;
        Ok(Some(parsed))
    }

    pub async fn get_job(
        &self,
        repo: Option<&str>,
        job_id: u64,
    ) -> Result<Option<GitHubJob>, GitHubError> {
        let target = self.resolve_repo(repo)?;
        self.request(
            Method::GET,
            &format!("/repos/{target}/actions/jobs/{job_id}"),
            None,
        )
        .await
    }

    pub async fn get_workflow_run(
        &self,
        repo: Option<&str>,
        run_id: u64,
    ) -> Result<Option<GitHubWorkflowRun>, GitHubError> {
        let target = self.resolve_repo(repo)?;
        self.request(
            Method::GET,
            &format!("/repos/{target}/actions/runs/{run_id}"),
            None,
        )
        .await
    }

    pub async fn list_run_attempt_jobs(
        &self,
        repo: Option<&str>,
        run_id: u64,
        attempt: u32,
    ) -> Result<Vec<GitHubJob>, GitHubError> {
        const MAX_PAGES: u32 = 50;
        let target = self.resolve_repo(repo)?;
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = format!(
                "/repos/{target}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100&page={page}"
            );
            let Some(resp): Option<JobsPage> = self.request(Method::GET, &path, None).await? else {
                break;
            };
            let count = resp.jobs.len();
            all.extend(resp.jobs);
            if count < 100 {
                break;
            }
        }
        Ok(all)
    }

    pub async fn list_runners(&self) -> Result<Vec<GitHubRunner>, GitHubError> {
        const MAX_PAGES: u32 = 50;
        let prefix = self.runners_prefix();
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = format!("{prefix}?per_page=100&page={page}");
            let Some(resp): Option<RunnersPage> = self.request(Method::GET, &path, None).await?
            else {
                break;
            };
            let count = resp.runners.len();
            all.extend(resp.runners);
            if count < 100 {
                break;
            }
        }
        Ok(all)
    }

    pub async fn delete_runner(&self, runner_id: u64) -> Result<(), GitHubError> {
        let prefix = self.runners_prefix();
        let _: Option<serde_json::Value> = self
            .request(Method::DELETE, &format!("{prefix}/{runner_id}"), None)
            .await?;
        Ok(())
    }

    pub async fn generate_jit_config(
        &self,
        runner_name: &str,
        architecture: Architecture,
        custom_label: &str,
    ) -> Result<SecretString, GitHubError> {
        let prefix = self.runners_prefix();
        let body = json!({
            "name": runner_name,
            "runner_group_id": self.scope.runner_group_id(),
            "labels": ["self-hosted", "Linux", architecture.github_label(), custom_label],
            "work_folder": "_work",
        });
        let resp: JitConfigResponse = self
            .request(
                Method::POST,
                &format!("{prefix}/generate-jitconfig"),
                Some(body),
            )
            .await?
            .ok_or_else(|| GitHubError::Api("empty jitconfig response".to_owned()))?;
        Ok(SecretString::new(resp.encoded_jit_config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{delete, get, post};
    use axum::{Json, Router, extract::Query};
    use std::collections::BTreeMap;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn paginates_run_jobs_across_multiple_pages() {
        let app = Router::new().route(
            "/repos/acme/app/actions/runs/77/attempts/1/jobs",
            get(|Query(q): Query<BTreeMap<String, String>>| async move {
                let page = q.get("page").map_or("1", String::as_str);
                if page == "1" {
                    let jobs: Vec<_> = (1..=100u64)
                        .map(|id| json!({ "id": id, "run_id": 77, "status": "queued", "labels": ["open-ci-api"] }))
                        .collect();
                    Json(json!({ "jobs": jobs }))
                } else {
                    Json(json!({ "jobs": [{ "id": 101, "run_id": 77, "status": "in_progress", "labels": ["open-ci-api"] }] }))
                }
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

                let gh = GitHubClient::new(
                    format!("http://{addr}"),
                    GitHubScope::Repository("acme/app".to_owned()),
                    Client::new(),
                    Arc::new(StaticGitHubToken(SecretString::new("ghs_test"))),
                );
                let jobs = gh.list_run_attempt_jobs(None, 77, 1).await;
                assert!(matches!(jobs, Ok(list) if list.len() == 101));
            }
        }
    }

    #[tokio::test]
    async fn org_scope_routes_runners_to_orgs_and_jobs_to_target_repo() {
        let recorded_group = Arc::new(Mutex::new(None::<u64>));
        let deleted_id = Arc::new(Mutex::new(None::<u64>));
        let g1 = Arc::clone(&recorded_group);
        let d1 = Arc::clone(&deleted_id);

        let app = Router::new()
            .route(
                "/orgs/acme/actions/runners",
                get(|| async {
                    Json(json!({
                        "runners": [{ "id": 501, "name": "open-ci-api-1", "status": "online", "busy": false }]
                    }))
                }),
            )
            .route(
                "/orgs/acme/actions/runners/generate-jitconfig",
                post(move |Json(body): Json<serde_json::Value>| {
                    let g = Arc::clone(&g1);
                    async move {
                        *g.lock().await = body["runner_group_id"].as_u64();
                        Json(json!({ "encoded_jit_config": "org-jit-token" }))
                    }
                }),
            )
            .route(
                "/orgs/acme/actions/runners/{id}",
                delete(move |axum::extract::Path(id): axum::extract::Path<u64>| {
                    let d = Arc::clone(&d1);
                    async move {
                        *d.lock().await = Some(id);
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .route(
                "/repos/acme/service-b/actions/jobs/88",
                get(|| async {
                    Json(json!({
                        "id": 88,
                        "run_id": 12,
                        "status": "queued",
                        "labels": ["self-hosted", "open-ci-api"]
                    }))
                }),
            )
            .route(
                "/repos/acme/service-b/actions/runs/12",
                get(|| async {
                    Json(json!({
                        "id": 12,
                        "run_attempt": 1,
                        "status": "completed",
                        "conclusion": "success"
                    }))
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

                let scope = GitHubScope::Organization {
                    org: "acme".to_owned(),
                    allowed_repositories: vec!["acme/*".to_owned()],
                    runner_group_id: Some(9),
                };
                let gh = GitHubClient::new(
                    format!("http://{addr}"),
                    scope,
                    Client::new(),
                    Arc::new(StaticGitHubToken(SecretString::new("ghs_test"))),
                );

                let runners = gh.list_runners().await;
                assert!(matches!(runners, Ok(ref list) if list.len() == 1 && list[0].id == 501));

                let jit = gh
                    .generate_jit_config("open-ci-api-1", Architecture::X64, "open-ci-api")
                    .await;
                assert!(matches!(jit, Ok(ref s) if s.expose() == "org-jit-token"));
                assert_eq!(*recorded_group.lock().await, Some(9));

                assert!(gh.delete_runner(501).await.is_ok());
                assert_eq!(*deleted_id.lock().await, Some(501));

                let job = gh.get_job(Some("acme/service-b"), 88).await;
                assert!(matches!(job, Ok(Some(j)) if j.id == 88 && j.run_id == 12));

                let run = gh.get_workflow_run(Some("acme/service-b"), 12).await;
                assert!(matches!(run, Ok(Some(r)) if r.status == "completed"));

                // Foreign org and missing repo are rejected client-side.
                assert!(gh.get_job(Some("evil/service-b"), 88).await.is_err());
                assert!(gh.get_job(None, 88).await.is_err());
            }
        }
    }
}
