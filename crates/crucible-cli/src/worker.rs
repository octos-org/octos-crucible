//! The Worker's internal endpoints, as GitHub Actions calls them
//! (docs/api.md, "给 GitHub Actions 的内部接口"). The bearer token comes from
//! `CRUCIBLE_WORKER_TOKEN` and is never printed.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crucible_core::netpolicy::validate_endpoint;
use reqwest::{Method, StatusCode};

pub const TOKEN_ENV: &str = "CRUCIBLE_WORKER_TOKEN";

pub struct Worker {
    client: reqwest::Client,
    base: String,
    token: String,
}

/// `https://host[:port]` of a URL (the Worker address of a `results_url`).
pub fn origin(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("Worker URL must be https"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains('@') {
        bail!("Worker URL has no host");
    }
    Ok(format!("https://{host}"))
}

impl Worker {
    pub fn new(worker_url: &str, token: String) -> Result<Worker> {
        let base = origin(worker_url.trim())?;
        validate_endpoint(&base, false).map_err(|e| anyhow!("Worker URL: {e}"))?;
        if token.trim().is_empty() {
            bail!("{TOKEN_ENV} is empty");
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("crucible/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Worker {
            client,
            base,
            token,
        })
    }

    pub fn from_env(worker_url: &str) -> Result<Worker> {
        let token = std::env::var(TOKEN_ENV).map_err(|_| anyhow!("{TOKEN_ENV} is not set"))?;
        Worker::new(worker_url, token)
    }

    /// One request, retried on network errors and 5xx (3 attempts).
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let url = format!("{}{path}", self.base);
        let mut last = None;
        for attempt in 1..=3u64 {
            let mut req = self
                .client
                .request(method.clone(), &url)
                .bearer_auth(&self.token);
            if let Some(b) = &body {
                req = req
                    .header("content-type", "application/json")
                    .body(b.clone());
            }
            match req.send().await {
                Ok(r) if !r.status().is_server_error() => {
                    let status = r.status();
                    return Ok((status, r.bytes().await?.to_vec()));
                }
                Ok(r) => last = Some(anyhow!("{method} {path}: HTTP {}", r.status())),
                Err(e) => last = Some(anyhow!("{method} {path}: {}", e.without_url())),
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_secs(3 * attempt)).await;
            }
        }
        Err(last.expect("at least one attempt"))
    }

    async fn expect_ok(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let (status, bytes) = self.call(method.clone(), path, body).await?;
        if !status.is_success() {
            // The Worker's error body is {"error":{"code","message"}}: no secrets.
            let msg = String::from_utf8_lossy(&bytes);
            bail!(
                "{method} {path}: HTTP {status}: {}",
                msg.chars().take(300).collect::<String>()
            );
        }
        Ok(bytes)
    }

    /// `GET /internal/cred/:id`: the sealed credential bytes.
    pub async fn get_cred(&self, eval_id: &str) -> Result<Vec<u8>> {
        self.expect_ok(Method::GET, &format!("/internal/cred/{eval_id}"), None)
            .await
            .context("fetching the credential from the Worker")
    }

    /// `DELETE /internal/cred/:id` (idempotent).
    pub async fn delete_cred(&self, eval_id: &str) -> Result<()> {
        self.expect_ok(Method::DELETE, &format!("/internal/cred/{eval_id}"), None)
            .await?;
        Ok(())
    }

    /// `GET /internal/tasksets/:id?github_id=N`: a user taskset's
    /// taskset.json, if `github_id` may use it (owner, or public).
    pub async fn get_user_taskset(&self, id: &str, github_id: u64) -> Result<Vec<u8>> {
        self.expect_ok(
            Method::GET,
            &format!("/internal/tasksets/{id}?github_id={github_id}"),
            None,
        )
        .await
        .with_context(|| format!("taskset {id} for user {github_id}"))
    }

    /// `POST /internal/tasksets/:id`: the outcome of `taskset-pack`.
    pub async fn taskset_result(&self, id: &str, body: &serde_json::Value) -> Result<()> {
        self.expect_ok(
            Method::POST,
            &format!("/internal/tasksets/{id}"),
            Some(serde_json::to_vec(body)?),
        )
        .await?;
        Ok(())
    }

    /// `GET /internal/plugins/:id?taskset=T`: an uploaded plugin as a
    /// taskset pins it, if the owner of taskset `T` may use it (their own
    /// ready plugin, or a public one).
    pub async fn get_user_plugin(
        &self,
        id: &str,
        taskset_id: &str,
    ) -> Result<crucible_core::plugins::UserPlugin> {
        let raw = self
            .expect_ok(
                Method::GET,
                &format!("/internal/plugins/{id}?taskset={taskset_id}"),
                None,
            )
            .await
            .with_context(|| format!("plugin {id}"))?;
        let p: crucible_core::plugins::UserPlugin = serde_json::from_slice(&raw)
            .map_err(|e| anyhow!("plugin {id} from the Worker: {e}"))?;
        if p.name != id {
            bail!("plugin {id} from the Worker is named {:?}", p.name);
        }
        p.check().map_err(|e| anyhow!("{e}"))?;
        Ok(p)
    }

    /// `POST /internal/plugins/:id`: the outcome of `plugin-pack`.
    pub async fn plugin_result(&self, id: &str, body: &serde_json::Value) -> Result<()> {
        self.expect_ok(
            Method::POST,
            &format!("/internal/plugins/{id}"),
            Some(serde_json::to_vec(body)?),
        )
        .await?;
        Ok(())
    }

    /// `POST /internal/status/:id`.
    pub async fn status(&self, eval_id: &str, status: &str) -> Result<()> {
        let body = serde_json::to_vec(&serde_json::json!({ "status": status }))?;
        self.expect_ok(
            Method::POST,
            &format!("/internal/status/{eval_id}"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    /// `POST /internal/results/:id`: the manifest (its `download` field is
    /// what the Worker serves), plus the final status.
    pub async fn results(
        &self,
        eval_id: &str,
        manifest: &serde_json::Value,
        status: &str,
    ) -> Result<()> {
        let mut body = manifest.clone();
        body.as_object_mut()
            .ok_or_else(|| anyhow!("manifest is not a JSON object"))?
            .insert("status".into(), status.into());
        self.expect_ok(
            Method::POST,
            &format!("/internal/results/{eval_id}"),
            Some(serde_json::to_vec(&body)?),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_of_results_url() {
        assert_eq!(
            origin("https://w.example.dev/internal/results/abc").unwrap(),
            "https://w.example.dev"
        );
        assert_eq!(
            origin("https://w.example.dev").unwrap(),
            "https://w.example.dev"
        );
        assert!(origin("http://w.example.dev/x").is_err());
        assert!(origin("https://u:p@w.example.dev/").is_err());
        assert!(Worker::new("https://w.example.dev", " ".into()).is_err());
        assert!(Worker::new("https://127.0.0.1", "t".into()).is_err());
    }
}
