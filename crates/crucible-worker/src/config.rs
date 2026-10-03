//! Worker configuration from `wrangler.toml` vars and secrets.

/// Default GitHub endpoints. They can only be overridden in dev mode, so a
/// production deployment can never be pointed at another server.
const GITHUB_API: &str = "https://api.github.com";
const GITHUB_WEB: &str = "https://github.com";

#[derive(Clone)]
pub struct Config {
    /// CORS origin of the web page, e.g. `https://octos-org.github.io`.
    pub pages_origin: String,
    /// Where login lands, e.g. `https://octos-org.github.io/octos-crucible/`.
    pub pages_url: String,
    /// `owner/name`.
    pub github_repo: String,
    pub admin_ids: Vec<u64>,
    /// Public base URL of this Worker, for `results_url`; the request
    /// origin when unset.
    pub worker_url: Option<String>,
    pub eval_workflow: String,
    pub eval_ref: String,
    pub github_api: String,
    pub github_web: String,
    pub client_id: String,
    pub client_secret: String,
    pub github_token: String,
    pub session_key: Vec<u8>,
    pub worker_token: String,
    /// `/auth/dev-login` and GitHub endpoint overrides; honoured only for
    /// requests to localhost.
    pub dev_auth: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secrets stay out of any debug output.
        f.debug_struct("Config")
            .field("pages_origin", &self.pages_origin)
            .field("github_repo", &self.github_repo)
            .field("admin_ids", &self.admin_ids)
            .field("dev_auth", &self.dev_auth)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// `get` returns a var or secret by name. The error names the bad
    /// setting, never its value.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let req = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{name} is not set"))
        };
        let opt = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };

        let dev_auth = opt("DEV_AUTH").as_deref() == Some("1");

        let pages_origin = req("PAGES_ORIGIN")?.trim_end_matches('/').to_owned();
        if !is_origin(&pages_origin, dev_auth) {
            return Err("PAGES_ORIGIN must be https://host[:port]".into());
        }
        let pages_url = opt("PAGES_URL").unwrap_or_else(|| format!("{pages_origin}/"));
        if !pages_url.starts_with(&format!("{pages_origin}/")) || pages_url.contains('#') {
            return Err("PAGES_URL must start with PAGES_ORIGIN/ and have no fragment".into());
        }

        let github_repo = req("GITHUB_REPO")?;
        if !valid_repo(&github_repo) {
            return Err("GITHUB_REPO must be owner/name".into());
        }

        let mut admin_ids = Vec::new();
        for part in opt("ADMIN_GITHUB_IDS").unwrap_or_default().split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            admin_ids.push(
                part.parse::<u64>()
                    .map_err(|_| "ADMIN_GITHUB_IDS must be comma-separated numbers".to_string())?,
            );
        }

        let worker_url = opt("WORKER_URL").map(|u| u.trim_end_matches('/').to_owned());
        if let Some(u) = &worker_url
            && !is_origin(u, dev_auth)
        {
            return Err("WORKER_URL must be https://host[:port]".into());
        }

        let eval_workflow = opt("EVAL_WORKFLOW").unwrap_or_else(|| "eval.yml".into());
        let eval_ref = opt("EVAL_REF").unwrap_or_else(|| "main".into());
        let safe = |s: &str| {
            !s.is_empty()
                && s.len() <= 100
                && !s.contains("..")
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
        };
        if !safe(&eval_workflow) || eval_workflow.contains('/') || !safe(&eval_ref) {
            return Err("EVAL_WORKFLOW / EVAL_REF contain unexpected characters".into());
        }

        let (github_api, github_web) = if dev_auth {
            (
                opt("GITHUB_API_BASE").unwrap_or_else(|| GITHUB_API.into()),
                opt("GITHUB_WEB_BASE").unwrap_or_else(|| GITHUB_WEB.into()),
            )
        } else {
            (GITHUB_API.into(), GITHUB_WEB.into())
        };

        let session_key = req("SESSION_HMAC_KEY")?.into_bytes();
        if session_key.len() < 32 {
            return Err("SESSION_HMAC_KEY must be at least 32 bytes".into());
        }
        let worker_token = req("CRUCIBLE_WORKER_TOKEN")?;
        if worker_token.len() < 32 {
            return Err("CRUCIBLE_WORKER_TOKEN must be at least 32 characters".into());
        }

        Ok(Config {
            pages_origin,
            pages_url,
            github_repo,
            admin_ids,
            worker_url,
            eval_workflow,
            eval_ref,
            github_api: github_api.trim_end_matches('/').to_owned(),
            github_web: github_web.trim_end_matches('/').to_owned(),
            client_id: req("GITHUB_CLIENT_ID")?,
            client_secret: req("GITHUB_CLIENT_SECRET")?,
            github_token: req("GITHUB_TOKEN")?,
            session_key,
            worker_token,
            dev_auth,
        })
    }

    pub fn is_admin(&self, github_id: u64) -> bool {
        self.admin_ids.contains(&github_id)
    }
}

fn is_origin(s: &str, allow_http: bool) -> bool {
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://").filter(|_| allow_http));
    match rest {
        Some(host) => {
            !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-.:".contains(&b))
        }
        None => false,
    }
}

/// Same rule as crucible-store's `valid_repo`.
pub fn valid_repo(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    matches!(repo.split_once('/'), Some((o, n)) if ok(o) && ok(n))
}

#[cfg(test)]
pub(crate) fn test_vars() -> Vec<(&'static str, &'static str)> {
    vec![
        ("PAGES_ORIGIN", "https://octos-org.github.io"),
        ("PAGES_URL", "https://octos-org.github.io/octos-crucible/"),
        ("GITHUB_REPO", "octos-org/octos-crucible"),
        ("ADMIN_GITHUB_IDS", "1, 2"),
        ("GITHUB_CLIENT_ID", "cid"),
        ("GITHUB_CLIENT_SECRET", "csecret"),
        ("GITHUB_TOKEN", "ghtoken"),
        ("SESSION_HMAC_KEY", "k-k-k-k-k-k-k-k-k-k-k-k-k-k-k-k-k-k"),
        (
            "CRUCIBLE_WORKER_TOKEN",
            "w-w-w-w-w-w-w-w-w-w-w-w-w-w-w-w-w-w",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(extra: &[(&'static str, &'static str)]) -> Result<Config, String> {
        let mut vars = test_vars();
        vars.extend_from_slice(extra);
        Config::from_lookup(|n| {
            vars.iter()
                .rev()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn parses() {
        let c = build(&[]).unwrap();
        assert_eq!(c.admin_ids, vec![1, 2]);
        assert!(c.is_admin(2) && !c.is_admin(3));
        assert_eq!(c.github_api, GITHUB_API);
        assert_eq!(c.eval_workflow, "eval.yml");
        assert!(!format!("{c:?}").contains("csecret"));
    }

    #[test]
    fn overrides_need_dev_mode() {
        let c = build(&[("GITHUB_API_BASE", "http://127.0.0.1:9999")]).unwrap();
        assert_eq!(c.github_api, GITHUB_API);
        let c = build(&[
            ("GITHUB_API_BASE", "http://127.0.0.1:9999"),
            ("DEV_AUTH", "1"),
        ])
        .unwrap();
        assert_eq!(c.github_api, "http://127.0.0.1:9999");
    }

    #[test]
    fn rejects() {
        assert!(build(&[("PAGES_ORIGIN", "http://evil")]).is_err());
        assert!(build(&[("PAGES_URL", "https://elsewhere.example/")]).is_err());
        assert!(build(&[("GITHUB_REPO", "nope")]).is_err());
        assert!(build(&[("ADMIN_GITHUB_IDS", "1,x")]).is_err());
        assert!(build(&[("SESSION_HMAC_KEY", "short")]).is_err());
        assert!(build(&[("EVAL_WORKFLOW", "../x.yml")]).is_err());
        let missing = Config::from_lookup(|_| None).unwrap_err();
        assert!(missing.contains("PAGES_ORIGIN"));
    }
}
