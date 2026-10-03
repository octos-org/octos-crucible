//! The GitHub calls the Worker makes: OAuth, release assets, workflow
//! dispatch, run status, taskset listing. Nothing here logs a token.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::http::{ApiError, Backend, HttpRequest, HttpResponse, PutOptions};
use crate::model::{BUILDING, FAILED, QUEUED, SCORING, is_slug};
use crate::util::{b64_decode_lenient, pct_encode, rfc3339};

const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = concat!("crucible-worker/", env!("CARGO_PKG_VERSION"));
/// Keeps `/tasksets` within the Worker's per-request subrequest budget.
const MAX_TASKSETS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub id: u64,
    /// RFC 6570 template, e.g. `https://uploads.github.com/repos/o/r/releases/1/assets{?name,label}`.
    pub upload_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadOutcome {
    Created,
    AlreadyExists,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunInfo {
    pub id: u64,
    pub html_url: String,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub display_title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageInfo {
    pub name: String,
    pub time_limit_s: u64,
    pub total: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TasksetInfo {
    pub name: String,
    pub version: String,
    pub stages: Vec<StageInfo>,
}

pub struct GitHub<'a, B: Backend> {
    pub b: &'a B,
    pub cfg: &'a Config,
}

fn upstream(what: &str, status: u16) -> ApiError {
    ApiError::upstream(format!("GitHub {what}: HTTP {status}"))
}

impl<'a, B: Backend> GitHub<'a, B> {
    fn req(&self, method: &'static str, url: String, token: Option<&str>) -> HttpRequest {
        let mut headers = vec![
            ("user-agent".to_string(), USER_AGENT.to_string()),
            (
                "accept".to_string(),
                "application/vnd.github+json".to_string(),
            ),
            ("x-github-api-version".to_string(), API_VERSION.to_string()),
        ];
        if let Some(t) = token {
            headers.push(("authorization".to_string(), format!("Bearer {t}")));
        }
        HttpRequest {
            method,
            url,
            headers,
            body: None,
        }
    }

    /// Repository-scoped API call with the Worker's token.
    fn repo(&self, method: &'static str, path: &str) -> HttpRequest {
        let url = format!(
            "{}/repos/{}/{path}",
            self.cfg.github_api, self.cfg.github_repo
        );
        self.req(method, url, Some(&self.cfg.github_token))
    }

    fn with_json(mut r: HttpRequest, body: &Value) -> HttpRequest {
        r.headers
            .push(("content-type".into(), "application/json".into()));
        r.body = Some(serde_json::to_vec(body).expect("json"));
        r
    }

    async fn send(&self, what: &str, r: HttpRequest) -> Result<HttpResponse, ApiError> {
        self.b.fetch(r).await.map_err(|e| {
            self.b.log(&format!("GitHub {what}: network error: {e}"));
            ApiError::upstream(format!("GitHub {what}: network error"))
        })
    }

    // ---- OAuth ----------------------------------------------------------

    pub fn authorize_url(&self, state: &str, redirect_uri: &str) -> String {
        format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}&state={}&allow_signup=true",
            self.cfg.github_web,
            pct_encode(&self.cfg.client_id),
            pct_encode(redirect_uri),
            pct_encode(state),
        )
    }

    /// Exchange the OAuth code for a user token (used once, then dropped).
    pub async fn oauth_token(&self, code: &str, redirect_uri: &str) -> Result<String, ApiError> {
        let url = format!("{}/login/oauth/access_token", self.cfg.github_web);
        let mut r = self.req("POST", url, None);
        r.headers.retain(|(k, _)| k != "accept");
        r.headers.push(("accept".into(), "application/json".into()));
        r.headers.push((
            "content-type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        r.body = Some(
            format!(
                "client_id={}&client_secret={}&code={}&redirect_uri={}",
                pct_encode(&self.cfg.client_id),
                pct_encode(&self.cfg.client_secret),
                pct_encode(code),
                pct_encode(redirect_uri)
            )
            .into_bytes(),
        );
        let resp = self.send("oauth token", r).await?;
        if resp.status != 200 {
            return Err(upstream("oauth token", resp.status));
        }
        resp.json()
            .and_then(|v| v.get("access_token")?.as_str().map(str::to_owned))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ApiError::new(401, "oauth_failed", "GitHub did not issue a token"))
    }

    /// `(id, login)` of the token's user.
    pub async fn user(&self, token: &str) -> Result<(u64, String), ApiError> {
        let url = format!("{}/user", self.cfg.github_api);
        let resp = self.send("user", self.req("GET", url, Some(token))).await?;
        if resp.status != 200 {
            return Err(upstream("user", resp.status));
        }
        let v = resp
            .json()
            .ok_or_else(|| ApiError::upstream("GitHub user: bad body"))?;
        let id = v.get("id").and_then(Value::as_u64);
        let login = v.get("login").and_then(Value::as_str);
        match (id, login) {
            (Some(id), Some(login)) if id > 0 && crate::model::is_login(login) => {
                Ok((id, login.to_owned()))
            }
            _ => Err(ApiError::upstream("GitHub user: unexpected body")),
        }
    }

    // ---- Blob store (与 crucible-store 保持一致) -------------------------

    /// The release for `tag`, created as a pre-release if missing. Cached
    /// in KV: tags never move.
    pub async fn release(&self, tag: &str) -> Result<Release, ApiError> {
        let cache_key = format!("cache/release/{tag}");
        if let Ok(Some(raw)) = self.b.kv_get(&cache_key).await
            && let Ok(r) = serde_json::from_slice::<Release>(&raw)
        {
            return Ok(r);
        }
        let mut found = self.fetch_release(tag).await?;
        if found.is_none() {
            let r = Self::with_json(
                self.repo("POST", "releases"),
                &serde_json::json!({
                    "tag_name": tag,
                    "name": tag,
                    "body": "Encrypted blob storage for octos-crucible. Asset name = SHA-256 of the asset.",
                    "prerelease": true,
                }),
            );
            let resp = self.send("create release", r).await?;
            found = match resp.status {
                201 => resp.json().and_then(|v| serde_json::from_value(v).ok()),
                // Lost a race with another writer creating the same shard.
                422 => self.fetch_release(tag).await?,
                s => return Err(upstream("create release", s)),
            };
        }
        let release = found
            .ok_or_else(|| ApiError::upstream(format!("release {tag} missing after create")))?;
        let _ = self
            .b
            .kv_put(
                &cache_key,
                &serde_json::to_vec(&release).expect("json"),
                PutOptions::default(),
            )
            .await;
        Ok(release)
    }

    async fn fetch_release(&self, tag: &str) -> Result<Option<Release>, ApiError> {
        let resp = self
            .send(
                "get release",
                self.repo("GET", &format!("releases/tags/{tag}")),
            )
            .await?;
        match resp.status {
            200 => Ok(resp.json().and_then(|v| serde_json::from_value(v).ok())),
            404 => Ok(None),
            s => Err(upstream("get release", s)),
        }
    }

    /// Upload `data` as asset `hash`. A 422 `already_exists` means the same
    /// bytes are already stored (put is idempotent).
    pub async fn upload_asset(
        &self,
        release: &Release,
        hash: &str,
        data: Vec<u8>,
    ) -> Result<UploadOutcome, ApiError> {
        let url = upload_url(&release.upload_url, hash)
            .ok_or_else(|| ApiError::upstream("GitHub release has a bad upload_url"))?;
        let mut r = self.req("POST", url, Some(&self.cfg.github_token));
        r.headers
            .push(("content-type".into(), "application/octet-stream".into()));
        r.body = Some(data);
        let resp = self.send("upload asset", r).await?;
        match resp.status {
            201 => Ok(UploadOutcome::Created),
            422 if String::from_utf8_lossy(&resp.body).contains("already_exists") => {
                Ok(UploadOutcome::AlreadyExists)
            }
            s => Err(upstream("upload asset", s)),
        }
    }

    /// Public download URL of a blob (assets of a public repo's releases
    /// need no token; blobs are sealed or password-protected anyway).
    pub fn asset_download_url(&self, tag: &str, hash: &str) -> String {
        format!(
            "{}/{}/releases/download/{tag}/{hash}",
            self.cfg.github_web, self.cfg.github_repo
        )
    }

    // ---- Actions --------------------------------------------------------

    pub async fn dispatch(&self, workflow: &str, inputs: &Value) -> Result<(), ApiError> {
        let r = Self::with_json(
            self.repo("POST", &format!("actions/workflows/{workflow}/dispatches")),
            &serde_json::json!({"ref": self.cfg.eval_ref, "inputs": inputs}),
        );
        let resp = self.send("workflow dispatch", r).await?;
        match resp.status {
            200 | 204 => Ok(()),
            s => Err(upstream("workflow dispatch", s)),
        }
    }

    /// The run whose `run-name` contains `eval_id` (the workflow's
    /// `run-name` must include `${{ inputs.eval_id }}`).
    pub async fn find_run(
        &self,
        workflow: &str,
        eval_id: &str,
        created_s: u64,
    ) -> Result<Option<RunInfo>, ApiError> {
        // Day granularity, one day early to absorb clock and timezone skew.
        let since = &rfc3339(created_s.saturating_sub(86_400))[..10];
        let path = format!(
            "actions/workflows/{workflow}/runs?event=workflow_dispatch&per_page=100&created={}",
            pct_encode(&format!(">={since}"))
        );
        let resp = self.send("list runs", self.repo("GET", &path)).await?;
        if resp.status != 200 {
            return Err(upstream("list runs", resp.status));
        }
        let runs: Vec<RunInfo> = resp
            .json()
            .and_then(|v| serde_json::from_value(v.get("workflow_runs")?.clone()).ok())
            .unwrap_or_default();
        Ok(runs.into_iter().find(|r| r.display_title.contains(eval_id)))
    }

    pub async fn run(&self, run_id: u64) -> Result<RunInfo, ApiError> {
        let resp = self
            .send(
                "get run",
                self.repo("GET", &format!("actions/runs/{run_id}")),
            )
            .await?;
        if resp.status != 200 {
            return Err(upstream("get run", resp.status));
        }
        resp.json()
            .and_then(|v| serde_json::from_value(v).ok())
            .ok_or_else(|| ApiError::upstream("GitHub get run: bad body"))
    }

    /// Name of the first job still in progress.
    pub async fn current_job(&self, run_id: u64) -> Result<Option<String>, ApiError> {
        let resp = self
            .send(
                "list jobs",
                self.repo("GET", &format!("actions/runs/{run_id}/jobs?per_page=100")),
            )
            .await?;
        if resp.status != 200 {
            return Err(upstream("list jobs", resp.status));
        }
        Ok(resp.json().and_then(|v| {
            v.get("jobs")?
                .as_array()?
                .iter()
                .find(|j| j.get("status").and_then(Value::as_str) == Some("in_progress"))
                .and_then(|j| j.get("name")?.as_str())
                .map(|n| n.chars().take(100).collect())
        }))
    }

    // ---- Tasksets -------------------------------------------------------

    /// `tasksets/*/taskset.json` on the default ref. Invalid tasksets are
    /// skipped (and logged).
    pub async fn tasksets(&self) -> Result<Vec<TasksetInfo>, ApiError> {
        let r = self.repo(
            "GET",
            &format!("contents/tasksets?ref={}", pct_encode(&self.cfg.eval_ref)),
        );
        let resp = self.send("list tasksets", r).await?;
        let dirs: Vec<String> = match resp.status {
            200 => resp
                .json()
                .and_then(|v| {
                    Some(
                        v.as_array()?
                            .iter()
                            .filter(|e| e.get("type").and_then(Value::as_str) == Some("dir"))
                            .filter_map(|e| e.get("name")?.as_str().map(str::to_owned))
                            .filter(|n| crate::model::is_slug(n))
                            .take(MAX_TASKSETS)
                            .collect(),
                    )
                })
                .unwrap_or_default(),
            404 => vec![],
            s => return Err(upstream("list tasksets", s)),
        };
        let mut out = Vec::new();
        for dir in dirs {
            let r = self.repo(
                "GET",
                &format!(
                    "contents/tasksets/{dir}/taskset.json?ref={}",
                    pct_encode(&self.cfg.eval_ref)
                ),
            );
            let resp = self.send("get taskset", r).await?;
            if resp.status != 200 {
                self.b
                    .log(&format!("taskset {dir}: HTTP {} (skipped)", resp.status));
                continue;
            }
            match resp.json().and_then(|v| parse_taskset_file(&dir, &v)) {
                Some(t) => out.push(t),
                None => self.b.log(&format!("taskset {dir}: invalid (skipped)")),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// The parts of `taskset.json` the page needs. Parsed locally rather than
/// through crucible-core's `TaskSet`, whose shape is still moving; unknown
/// fields are ignored.
#[derive(Deserialize)]
struct TasksetFile {
    name: String,
    #[serde(default)]
    version: Option<Value>,
    stages: Vec<StageFile>,
}

#[derive(Deserialize)]
struct StageFile {
    id: String,
    time_limit_s: u64,
    #[serde(default)]
    expected_total: Option<u32>,
}

/// A contents-API file object → taskset summary. The taskset's name must
/// equal its directory. `version` is the file's own `version` field when
/// present, else `git-<blob sha prefix>`.
pub fn parse_taskset_file(dir: &str, file: &Value) -> Option<TasksetInfo> {
    let raw = b64_decode_lenient(file.get("content")?.as_str()?)?;
    let ts: TasksetFile = serde_json::from_slice(&raw).ok()?;
    let stages_ok = !ts.stages.is_empty()
        && ts.stages.len() <= 50
        && ts
            .stages
            .iter()
            .all(|s| is_slug(&s.id) && s.time_limit_s > 0);
    if ts.name != dir || !is_slug(&ts.name) || !stages_ok {
        return None;
    }
    let version = match ts.version {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= 64 => s,
        Some(Value::Number(n)) => n.to_string(),
        _ => format!(
            "git-{}",
            file.get("sha")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .chars()
                .take(12)
                .collect::<String>()
        ),
    };
    Some(TasksetInfo {
        name: ts.name,
        version,
        stages: ts
            .stages
            .into_iter()
            .map(|s| StageInfo {
                name: s.id,
                time_limit_s: s.time_limit_s,
                total: s.expected_total,
            })
            .collect(),
    })
}

/// `upload_url` without its `{?name,label}` template, plus `?name=`.
pub fn upload_url(template: &str, name: &str) -> Option<String> {
    let base = template.split('{').next().unwrap_or(template);
    if !(base.starts_with("https://") || base.starts_with("http://")) || base.contains('?') {
        return None;
    }
    Some(format!("{base}?name={}", pct_encode(name)))
}

/// What GitHub says about a run that has not delivered results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunView {
    /// A status estimated from the run and its current job.
    Status(String),
    /// Finished successfully, but results have not arrived (yet).
    CompletedOk,
}

/// GitHub run state (+ the in-progress job's name) → eval status. Only a
/// fallback: the workflow reports precise progress via
/// `POST /internal/status/:id`. Job names: `generate*`/`run*` →
/// `running:<first stage>`, `score*`/`publish*` → scoring, anything else
/// (setup, build) → building.
pub fn view_run(run: &RunInfo, job: Option<&str>, first_stage: Option<&str>) -> RunView {
    match run.status.as_str() {
        "completed" => match run.conclusion.as_deref() {
            Some("success") => RunView::CompletedOk,
            _ => RunView::Status(FAILED.into()),
        },
        "in_progress" => {
            let job = job.unwrap_or_default().to_ascii_lowercase();
            let s = if job.starts_with("score") || job.starts_with("publish") {
                SCORING.to_owned()
            } else if job.starts_with("generate") || job.starts_with("run") {
                match first_stage {
                    Some(stage) => format!("running:{stage}"),
                    None => BUILDING.to_owned(),
                }
            } else {
                BUILDING.to_owned()
            };
            RunView::Status(s)
        }
        _ => RunView::Status(QUEUED.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::b64_encode;

    #[test]
    fn upload_urls() {
        let h = "ab".repeat(32);
        assert_eq!(
            upload_url(
                "https://uploads.github.com/repos/o/r/releases/1/assets{?name,label}",
                &h
            )
            .unwrap(),
            format!("https://uploads.github.com/repos/o/r/releases/1/assets?name={h}")
        );
        assert!(upload_url("javascript:alert(1)", &h).is_none());
    }

    #[test]
    fn run_mapping() {
        let run = |status: &str, conclusion: Option<&str>| RunInfo {
            id: 1,
            html_url: String::new(),
            status: status.into(),
            conclusion: conclusion.map(str::to_owned),
            display_title: String::new(),
        };
        let st = |s: &str| RunView::Status(s.into());
        let busy = run("in_progress", None);
        assert_eq!(view_run(&run("queued", None), None, None), st("queued"));
        assert_eq!(view_run(&busy, Some("setup"), Some("stage-1")), st("building"));
        assert_eq!(
            view_run(&busy, Some("generate r1"), Some("stage-1")),
            st("running:stage-1")
        );
        assert_eq!(view_run(&busy, Some("publish"), None), st("scoring"));
        let done = |c: &str| view_run(&run("completed", Some(c)), None, None);
        assert_eq!(done("success"), RunView::CompletedOk);
        assert_eq!(done("failure"), st("failed"));
        assert_eq!(done("cancelled"), st("failed"));
    }

    #[test]
    fn taskset_files() {
        let json = r#"{"schema":1,"name":"github-full","version":"2026.10","scorer":{"name":"playwright"},
          "stages":[{"id":"stage-1","inputs":["stage-1/requirements.md"],"output":"web_app","time_limit_s":3600,"expected_total":30}]}"#;
        let file =
            serde_json::json!({"sha": "0123456789abcdef", "content": b64_encode(json.as_bytes())});
        let t = parse_taskset_file("github-full", &file).unwrap();
        assert_eq!(t.version, "2026.10");
        assert_eq!(
            t.stages,
            vec![StageInfo {
                name: "stage-1".into(),
                time_limit_s: 3600,
                total: Some(30)
            }]
        );
        assert!(parse_taskset_file("other-dir", &file).is_none());

        let no_version = json.replace(r#""version":"2026.10","#, "");
        let file = serde_json::json!({"sha": "0123456789abcdef", "content": b64_encode(no_version.as_bytes())});
        assert_eq!(
            parse_taskset_file("github-full", &file).unwrap().version,
            "git-0123456789ab"
        );

        let bad_stage = json.replace(r#""id":"stage-1""#, r#""id":"Stage 1""#);
        let file = serde_json::json!({"sha": "x", "content": b64_encode(bad_stage.as_bytes())});
        assert!(parse_taskset_file("github-full", &file).is_none());
    }

    /// The step-2 format (inputs_blob/tests_blob, kebab-case output,
    /// total_time_limit_s) parses too.
    #[test]
    fn taskset_step2_format() {
        let json = r#"{"schema":1,"name":"arcbench-github","scorer":{"name":"playwright"},"aggregate":"sum",
          "total_time_limit_s":15600,"stages":[
          {"id":"stage-1","inputs_blob":{"sha256":"aa","key_id":"k"},"tests_blob":{"sha256":"bb","key_id":"k"},"output":"web-app","time_limit_s":4800,"expected_total":30},
          {"id":"stage-2","inputs_blob":{"sha256":"aa","key_id":"k"},"tests_blob":{"sha256":"bb","key_id":"k"},"output":"web-app","time_limit_s":4800}]}"#;
        let file = serde_json::json!({"sha": "fedcba9876543210", "content": b64_encode(json.as_bytes())});
        let t = parse_taskset_file("arcbench-github", &file).unwrap();
        assert_eq!(t.stages.len(), 2);
        assert_eq!(t.stages[1].total, None);
        assert_eq!(t.version, "git-fedcba987654");
    }
}
