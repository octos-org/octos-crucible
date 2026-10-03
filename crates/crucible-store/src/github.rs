//! Blobs as assets of 32 pre-release GitHub Releases `blobs-00`…`blobs-31`;
//! the asset name is the full hex SHA-256.

use std::collections::HashMap;
use std::sync::Mutex;

use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use url::Url;

use crate::{BlobStore, StoreError, check_hash, release_tag, sha256_hex, verify};

const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = concat!("crucible-store/", env!("CARGO_PKG_VERSION"));
const PAGE: usize = 100;

#[derive(Debug, Clone, Deserialize)]
struct Release {
    id: u64,
    /// RFC 6570 template, e.g. `https://uploads.github.com/repos/o/r/releases/1/assets{?name,label}`.
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct Asset {
    id: u64,
    name: String,
}

pub struct GithubReleaseStore {
    client: reqwest::Client,
    api: Url,
    repo: String,
    token: String,
    /// Release per tag; tags never move, so caching for the process
    /// lifetime is safe.
    releases: Mutex<HashMap<String, Release>>,
}

impl GithubReleaseStore {
    /// `repo` is `owner/name`. The token needs `contents: write` to put.
    pub fn new(repo: &str, token: String) -> Result<Self, StoreError> {
        Self::with_api_base(repo, token, "https://api.github.com")
    }

    /// Token from `GITHUB_TOKEN`.
    pub fn from_env(repo: &str) -> Result<Self, StoreError> {
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| StoreError::Config("GITHUB_TOKEN is not set".into()))?;
        Self::new(repo, token)
    }

    /// For tests against a mock server.
    pub fn with_api_base(repo: &str, token: String, api_base: &str) -> Result<Self, StoreError> {
        if !valid_repo(repo) {
            return Err(StoreError::Config(format!(
                "repo must be owner/name, got {repo:?}"
            )));
        }
        let api = Url::parse(api_base).map_err(|e| StoreError::Config(e.to_string()))?;
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| StoreError::Config(e.to_string()))?;
        Ok(GithubReleaseStore {
            client,
            api,
            repo: repo.to_owned(),
            token,
            releases: Mutex::default(),
        })
    }

    pub(crate) fn api_url(&self, path: &str) -> Url {
        let base = self.api.as_str().trim_end_matches('/');
        Url::parse(&format!("{base}/repos/{}/{path}", self.repo)).expect("valid API URL")
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        self.request_accept(method, url, "application/vnd.github+json")
    }

    fn request_accept(&self, method: Method, url: Url, accept: &str) -> RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", API_VERSION)
    }

    /// The release for `tag`, created as a pre-release when `create` is set
    /// and it does not exist yet. `None` if absent and not created.
    async fn release(&self, tag: &str, create: bool) -> Result<Option<Release>, StoreError> {
        if let Some(r) = self.releases.lock().unwrap().get(tag) {
            return Ok(Some(r.clone()));
        }
        let mut found = self.fetch_release(tag).await?;
        if found.is_none() && create {
            let resp = self
                .request(Method::POST, self.api_url("releases"))
                .json(&serde_json::json!({
                    "tag_name": tag,
                    "name": tag,
                    "body": "Encrypted blob storage for octos-crucible. Asset name = SHA-256 of the asset.",
                    "prerelease": true,
                }))
                .send()
                .await
                .map_err(net)?;
            found = match resp.status() {
                StatusCode::CREATED => Some(resp.json().await.map_err(net)?),
                // Lost a race with another writer creating the same shard.
                StatusCode::UNPROCESSABLE_ENTITY => self.fetch_release(tag).await?,
                s => return Err(api_error("create release", s)),
            };
        }
        if let Some(r) = &found {
            self.releases
                .lock()
                .unwrap()
                .insert(tag.to_owned(), r.clone());
        }
        Ok(found)
    }

    async fn fetch_release(&self, tag: &str) -> Result<Option<Release>, StoreError> {
        let resp = self
            .request(Method::GET, self.api_url(&format!("releases/tags/{tag}")))
            .send()
            .await
            .map_err(net)?;
        match resp.status() {
            StatusCode::OK => Ok(Some(resp.json().await.map_err(net)?)),
            StatusCode::NOT_FOUND => Ok(None),
            s => Err(api_error("get release", s)),
        }
    }

    async fn find_asset(&self, release: &Release, name: &str) -> Result<Option<u64>, StoreError> {
        for page in 1.. {
            let mut url = self.api_url(&format!("releases/{}/assets", release.id));
            url.query_pairs_mut()
                .append_pair("per_page", &PAGE.to_string())
                .append_pair("page", &page.to_string());
            let resp = self.request(Method::GET, url).send().await.map_err(net)?;
            if resp.status() != StatusCode::OK {
                return Err(api_error("list assets", resp.status()));
            }
            let assets: Vec<Asset> = resp.json().await.map_err(net)?;
            if let Some(a) = assets.iter().find(|a| a.name == name) {
                return Ok(Some(a.id));
            }
            if assets.len() < PAGE {
                break;
            }
        }
        Ok(None)
    }

    async fn locate(&self, hash: &str) -> Result<Option<u64>, StoreError> {
        let Some(release) = self.release(&release_tag(hash)?, false).await? else {
            return Ok(None);
        };
        self.find_asset(&release, hash).await
    }
}

/// `upload_url` without its `{?name,label}` template, plus `?name=`.
pub(crate) fn upload_url(template: &str, name: &str) -> Result<Url, StoreError> {
    let base = template.split('{').next().unwrap_or(template);
    let mut url = Url::parse(base).map_err(|_| StoreError::Api("bad upload_url".into()))?;
    url.query_pairs_mut().append_pair("name", name);
    Ok(url)
}

fn valid_repo(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    matches!(repo.split_once('/'), Some((o, n)) if ok(o) && ok(n))
}

/// Network errors: the class only, never a message that might carry a URL
/// with a token-bearing redirect.
fn net(e: reqwest::Error) -> StoreError {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect failed"
    } else if e.is_decode() {
        "unexpected response body"
    } else {
        "request failed"
    };
    StoreError::Api(kind.into())
}

fn api_error(what: &str, status: StatusCode) -> StoreError {
    StoreError::Api(format!("{what}: HTTP {status}"))
}

impl BlobStore for GithubReleaseStore {
    async fn put(&self, data: &[u8]) -> Result<String, StoreError> {
        let hash = sha256_hex(data);
        let tag = release_tag(&hash)?;
        let release = self
            .release(&tag, true)
            .await?
            .ok_or_else(|| StoreError::Api(format!("release {tag} missing after create")))?;
        if self.find_asset(&release, &hash).await?.is_some() {
            return Ok(hash);
        }
        let resp = self
            .request(Method::POST, upload_url(&release.upload_url, &hash)?)
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await
            .map_err(net)?;
        match resp.status() {
            StatusCode::CREATED => Ok(hash),
            // already_exists: a concurrent put of the same bytes won.
            StatusCode::UNPROCESSABLE_ENTITY => Ok(hash),
            s => Err(api_error("upload asset", s)),
        }
    }

    async fn get(&self, hash: &str) -> Result<Vec<u8>, StoreError> {
        check_hash(hash)?;
        let id = self
            .locate(hash)
            .await?
            .ok_or_else(|| StoreError::NotFound(hash.to_owned()))?;
        // GitHub answers with a redirect to its CDN; reqwest drops the
        // Authorization header when the redirect leaves the API host.
        let resp = self
            .request_accept(
                Method::GET,
                self.api_url(&format!("releases/assets/{id}")),
                "application/octet-stream",
            )
            .send()
            .await
            .map_err(net)?;
        if resp.status() != StatusCode::OK {
            return Err(api_error("download asset", resp.status()));
        }
        verify(hash, resp.bytes().await.map_err(net)?.to_vec())
    }

    async fn exists(&self, hash: &str) -> Result<bool, StoreError> {
        check_hash(hash)?;
        Ok(self.locate(hash).await?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let s = GithubReleaseStore::new("octos-org/octos-crucible", "t".into()).unwrap();
        assert_eq!(
            s.api_url("releases/tags/blobs-07").as_str(),
            "https://api.github.com/repos/octos-org/octos-crucible/releases/tags/blobs-07"
        );
        let h = sha256_hex(b"x");
        assert_eq!(
            upload_url(
                "https://uploads.github.com/repos/o/r/releases/42/assets{?name,label}",
                &h
            )
            .unwrap()
            .as_str(),
            format!("https://uploads.github.com/repos/o/r/releases/42/assets?name={h}")
        );
        let s =
            GithubReleaseStore::with_api_base("o/r", "t".into(), "http://127.0.0.1:9/").unwrap();
        assert_eq!(
            s.api_url("releases").as_str(),
            "http://127.0.0.1:9/repos/o/r/releases"
        );
    }

    #[test]
    fn repo_names() {
        for ok in ["o/r", "octos-org/octos-crucible", "a_b/c.d"] {
            assert!(valid_repo(ok), "{ok}");
        }
        for bad in ["", "o", "o/", "/r", "o/r/x", "o/..", "o/r?x", "o r/x"] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }
}
