//! The user's model endpoint and the HTTP client that reaches it.
//!
//! SSRF guard: the client resolves names through [`PublicOnlyResolver`],
//! which refuses the whole answer if any address is not public. reqwest then
//! connects to exactly the addresses the resolver returned, so a DNS answer
//! that changes between check and connect (rebinding) cannot slip through.
//! IP-literal hosts never reach a resolver; `validate_endpoint` checks them.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crucible_core::netpolicy::{is_public_ip, validate_endpoint};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::Deserialize;
use url::Url;

use crate::ConfigError;

/// Read-timeout between bytes from upstream; long generations stream slowly.
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(1800);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// `{"api_key", "endpoint"}` as handed over on stdin.
#[derive(Deserialize)]
pub struct Credential {
    pub api_key: String,
    pub endpoint: String,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential { .. }")
    }
}

pub struct Upstream {
    chat_url: Url,
    api_key: String,
}

impl fmt::Debug for Upstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Upstream { .. }")
    }
}

impl Upstream {
    /// `allow_insecure` permits http and non-public addresses. Only the
    /// in-process integration tests set it; no CLI flag reaches it.
    pub fn new(cred: Credential, allow_insecure: bool) -> Result<Self, ConfigError> {
        let base = validate_endpoint(&cred.endpoint, allow_insecure)?;
        if cred.api_key.trim().is_empty() {
            return Err(ConfigError::NoKey);
        }
        let mut chat_url = base;
        let path = format!("{}/chat/completions", chat_url.path().trim_end_matches('/'));
        chat_url.set_path(&path);
        Ok(Upstream {
            chat_url,
            api_key: cred.api_key.trim().to_owned(),
        })
    }

    pub(crate) fn chat_url(&self) -> &Url {
        &self.chat_url
    }

    pub(crate) fn bearer(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}

/// Marker error so a refused resolution can be told apart from a network
/// failure without looking at messages (which may name the host).
#[derive(Debug)]
pub(crate) struct BlockedAddress;

impl fmt::Display for BlockedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("meter policy: upstream resolves to a non-public address")
    }
}

impl std::error::Error for BlockedAddress {}

pub(crate) struct PublicOnlyResolver {
    allow_any: bool,
}

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_any = self.allow_any;
        Box::pin(async move {
            // Port is ignored by reqwest; it substitutes the URL's port.
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err(std::io::Error::other("upstream host did not resolve").into());
            }
            // Refuse the whole answer if any address is private: a mixed
            // answer is how rebinding attacks hedge.
            if !allow_any && addrs.iter().any(|a| !is_public_ip(a.ip())) {
                return Err(BlockedAddress.into());
            }
            let addrs: Addrs = Box::new(addrs.into_iter());
            Ok(addrs)
        })
    }
}

pub(crate) fn build_client(allow_insecure: bool) -> Result<reqwest::Client, ConfigError> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(PublicOnlyResolver {
            allow_any: allow_insecure,
        }))
        // Never honour HTTP(S)_PROXY from the environment, never follow a
        // redirect (it could point at an IP literal, bypassing the resolver).
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(!allow_insecure)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(UPSTREAM_READ_TIMEOUT)
        .build()
        .map_err(|_| ConfigError::Client)
}

/// Error class for the log. Never the error's message: reqwest's include
/// the URL.
pub(crate) fn classify(err: &reqwest::Error) -> &'static str {
    let mut src: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = src {
        if e.is::<BlockedAddress>() || e.to_string().contains("meter policy:") {
            return "blocked_address";
        }
        src = e.source();
    }
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else {
        "request"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(endpoint: &str, key: &str) -> Credential {
        Credential {
            api_key: key.into(),
            endpoint: endpoint.into(),
        }
    }

    #[test]
    fn chat_url_joins_base_path() {
        let up = Upstream::new(cred("https://api.example.com/api/paas/v4/", "k"), false).unwrap();
        assert_eq!(
            up.chat_url().as_str(),
            "https://api.example.com/api/paas/v4/chat/completions"
        );
        let up = Upstream::new(cred("https://api.example.com", "k"), false).unwrap();
        assert_eq!(up.chat_url().path(), "/chat/completions");
    }

    #[test]
    fn needs_key_and_valid_endpoint() {
        assert!(matches!(
            Upstream::new(cred("https://api.example.com/v1", " "), false),
            Err(ConfigError::NoKey)
        ));
        assert!(Upstream::new(cred("http://api.example.com/v1", "k"), false).is_err());
        assert!(Upstream::new(cred("https://127.0.0.1/v1", "k"), false).is_err());
    }

    #[test]
    fn debug_hides_secrets() {
        let c = cred("https://api.example.com/v1", "sk-SECRET");
        assert!(!format!("{c:?}").contains("SECRET"));
        let up = Upstream::new(c, false).unwrap();
        assert!(!format!("{up:?}").contains("SECRET"));
        assert!(!format!("{up:?}").contains("example"));
    }

    #[tokio::test]
    async fn resolver_refuses_private_names() {
        let r = PublicOnlyResolver { allow_any: false };
        for host in ["localhost", "127.0.0.1", "10.0.0.1", "169.254.169.254"] {
            let err = match r.resolve(host.parse().unwrap()).await {
                Ok(_) => panic!("{host} resolved"),
                Err(e) => e,
            };
            assert!(err.is::<BlockedAddress>(), "{host}: {err}");
        }
        let r = PublicOnlyResolver { allow_any: true };
        assert!(r.resolve("localhost".parse().unwrap()).await.is_ok());
    }
}
