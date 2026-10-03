//! Platform-neutral request/response types and the runtime interface.
//!
//! The handlers in [`crate::app`] only see these types, so the same code
//! runs in the Worker (wasm32, see `crate::wasm`) and in native tests with
//! an in-memory backend.

use serde::Serialize;
use serde_json::Value;

/// An incoming request with its body already read.
#[derive(Debug, Clone, Default)]
pub struct Req {
    /// Upper case.
    pub method: String,
    /// `https://worker.example.dev` (scheme + host + port).
    pub origin: String,
    /// Host without port.
    pub host: String,
    pub path: String,
    /// Raw query string without `?`.
    pub query: String,
    /// Names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn bearer(&self) -> Option<&str> {
        let v = self.header("authorization")?;
        let (scheme, token) = v.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
    }

    /// Value of one cookie (only used for the OAuth state nonce).
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?.split(';').find_map(|c| {
            let (k, v) = c.trim().split_once('=')?;
            (k == name).then_some(v)
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json<T: Serialize>(status: u16, value: &T) -> Resp {
        Resp {
            status,
            headers: vec![(
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            )],
            body: serde_json::to_vec(value).expect("response serializes"),
        }
    }

    pub fn bytes(body: Vec<u8>) -> Resp {
        Resp {
            status: 200,
            headers: vec![("content-type".into(), "application/octet-stream".into())],
            body,
        }
    }

    pub fn empty(status: u16) -> Resp {
        Resp {
            status,
            headers: vec![],
            body: vec![],
        }
    }

    pub fn redirect(location: &str) -> Resp {
        Resp {
            status: 302,
            headers: vec![("location".into(), location.into())],
            body: vec![],
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Resp {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Every error leaves as `{"error":{"code","message"}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, "bad_request", message)
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "unauthorized", message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "forbidden", message)
    }
    pub fn banned() -> Self {
        Self::new(403, "banned", "this account is banned")
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found", message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, "conflict", message)
    }
    pub fn too_large(limit: usize) -> Self {
        Self::new(
            413,
            "payload_too_large",
            format!("body exceeds {limit} bytes"),
        )
    }
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(502, "upstream_error", message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "internal", message)
    }

    pub fn into_resp(self) -> Resp {
        Resp::json(
            self.status,
            &serde_json::json!({"error": {"code": self.code, "message": self.message}}),
        )
    }
}

/// An outgoing request (GitHub API / OAuth).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    /// Seconds; Workers KV requires at least 60.
    pub ttl: Option<u64>,
    /// Small JSON (< 1 KiB) returned by `kv_list`.
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct KvKey {
    pub name: String,
    pub metadata: Option<Value>,
}

/// What the handlers need from the runtime. Errors are plain strings that
/// never contain request bodies or secrets.
#[allow(async_fn_in_trait)] // single-threaded runtime; no Send bound wanted
pub trait Backend {
    async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    async fn kv_put(&self, key: &str, value: &[u8], opts: PutOptions) -> Result<(), String>;
    async fn kv_delete(&self, key: &str) -> Result<(), String>;
    /// Up to `limit` keys under `prefix`, with their metadata.
    async fn kv_list(&self, prefix: &str, limit: usize) -> Result<Vec<KvKey>, String>;
    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, String>;
    fn now_s(&self) -> u64;
    fn random_bytes(&self, n: usize) -> Vec<u8>;
    /// Operational log line. Callers never pass tokens, envelopes or bodies.
    fn log(&self, line: &str);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers() {
        let r = Req {
            headers: vec![
                ("authorization".into(), "Bearer abc.def".into()),
                ("cookie".into(), "a=1; crucible_oauth=n0nce; b=2".into()),
            ],
            ..Default::default()
        };
        assert_eq!(r.bearer(), Some("abc.def"));
        assert_eq!(r.cookie("crucible_oauth"), Some("n0nce"));
        assert_eq!(r.cookie("missing"), None);
        let basic = Req {
            headers: vec![("authorization".into(), "Basic xyz".into())],
            ..Default::default()
        };
        assert_eq!(basic.bearer(), None);
    }

    #[test]
    fn error_shape() {
        let r = ApiError::not_found("no such eval").into_resp();
        assert_eq!(r.status, 404);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["error"]["code"], "not_found");
        assert_eq!(v["error"]["message"], "no such eval");
    }
}
