//! The meter against a local fake upstream; scenarios of the prototype's
//! tools/tests/test_meter_proxy.py.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream as StdTcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_core::UsageRecord;
use crucible_meter::{Credential, Limits, MeterConfig, Upstream, serve};
use crucible_metering::Pricing;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const KEY: &str = "sk-test-SECRET-0123456789";
const MODEL: &str = "glm-5.3-flash";

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

type SeenLog = Arc<Mutex<Vec<Seen>>>;

fn usage(prompt: u64, cached: u64, completion: u64, reasoning: u64) -> Value {
    json!({"prompt_tokens": prompt, "completion_tokens": completion,
           "total_tokens": prompt + completion,
           "prompt_tokens_details": {"cached_tokens": cached},
           "completion_tokens_details": {"reasoning_tokens": reasoning}})
}

/// Minimal HTTP/1.1 upstream on a raw socket, so the test controls write
/// boundaries exactly (SSE deliberately split mid-line, close-delimited).
async fn fake_upstream(seen: SeenLog) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                let mut line = String::new();
                r.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut headers = HashMap::new();
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).await.unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    headers.insert(k.trim().to_lowercase(), v.trim().to_owned());
                }
                let len: usize = headers["content-length"].parse().unwrap();
                let mut buf = vec![0; len];
                r.read_exact(&mut buf).await.unwrap();
                let body: Value = serde_json::from_slice(&buf).unwrap();
                seen.lock().unwrap().push(Seen {
                    path,
                    headers,
                    body: body.clone(),
                });
                let mode = body["user"].as_str().unwrap_or("").to_owned();
                if let Some(code) = mode.strip_prefix("status-") {
                    let data =
                        json!({"error": {"message": format!("upstream {code}")}}).to_string();
                    let head = format!(
                        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = w.write_all(head.as_bytes()).await;
                    let _ = w.write_all(data.as_bytes()).await;
                    return;
                }
                if body["stream"] != json!(true) {
                    let data = json!({"id": "x", "model": MODEL,
                        "choices": [{"message": {"content": "hi"}}],
                        "usage": usage(1000, 600, 50, 7)})
                    .to_string();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        data.len()
                    );
                    let _ = w.write_all(head.as_bytes()).await;
                    let _ = w.write_all(data.as_bytes()).await;
                    return;
                }
                // SSE, close-delimited. Usage only in the last chunk and only
                // when stream_options.include_usage was asked for.
                let slow = mode == "slow";
                let n = if slow { 200 } else { 3 };
                let mut events: Vec<Value> = (0..n)
                    .map(|i| json!({"model": MODEL, "choices": [{"delta": {"content": format!("t{i}")}}], "usage": null}))
                    .collect();
                if body["stream_options"]["include_usage"] == json!(true) {
                    events.push(
                        json!({"model": MODEL, "choices": [], "usage": usage(2000, 1500, 120, 0)}),
                    );
                }
                let mut raw = Vec::new();
                for e in events {
                    raw.extend_from_slice(format!("data: {e}\n\n").as_bytes());
                }
                raw.extend_from_slice(b"data: [DONE]\n\n");
                let _ = w
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                    .await;
                for piece in raw.chunks(37) {
                    if w.write_all(piece).await.is_err() {
                        return;
                    }
                    let _ = w.flush().await;
                    if slow {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            });
        }
    });
    port
}

struct Fixture {
    port: u16,
    upstream_port: u16,
    log: PathBuf,
    seen: SeenLog,
    server: tokio::task::JoinHandle<()>,
    _tmp: tempfile::TempDir,
}

fn pricing() -> Pricing {
    Pricing::from_json(include_str!("../../../config/pricing.json")).unwrap()
}

async fn fixture(limits: Limits) -> Fixture {
    let seen: SeenLog = Arc::default();
    let upstream_port = fake_upstream(seen.clone()).await;
    let endpoint = format!("http://127.0.0.1:{upstream_port}/api/paas/v4");
    start(endpoint, true, limits, seen, upstream_port).await
}

async fn start(
    endpoint: String,
    insecure: bool,
    limits: Limits,
    seen: SeenLog,
    upstream_port: u16,
) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("usage.jsonl");
    let cfg = MeterConfig {
        upstream: Upstream::new(
            Credential {
                api_key: KEY.into(),
                endpoint,
            },
            insecure,
        )
        .unwrap(),
        model: MODEL.into(),
        pricing: pricing(),
        user_price: None,
        force_usage: true,
        limits,
        log_path: log.clone(),
        insecure_allow_loopback_for_tests: insecure,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        serve(listener, cfg).await.unwrap();
    });
    Fixture {
        port,
        upstream_port,
        log,
        seen,
        server,
        _tmp: tmp,
    }
}

impl Fixture {
    async fn post(&self, body: Value) -> (u16, Vec<u8>, reqwest::header::HeaderMap) {
        self.post_path("/chat/completions", body).await
    }

    async fn post_path(
        &self,
        path: &str,
        body: Value,
    ) -> (u16, Vec<u8>, reqwest::header::HeaderMap) {
        let r = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}{path}", self.port))
            .header("Authorization", "Bearer dummy")
            .header("X-Leak", "nope")
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        (status, r.bytes().await.unwrap().to_vec(), headers)
    }

    async fn records(&self) -> Vec<UsageRecord> {
        // The streaming record is written by the relay task right after the
        // body ends; give it a moment.
        tokio::time::sleep(Duration::from_millis(100)).await;
        read_records(&self.log)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn read_records(path: &Path) -> Vec<UsageRecord> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn msgs() -> Value {
    json!([{"role": "user", "content": "the-secret-prompt"}])
}

#[tokio::test]
async fn non_streaming() {
    let px = fixture(Limits::default()).await;
    let (status, data, _) = px
        .post(json!({"model": MODEL, "messages": msgs(), "tools": [{}, {}]}))
        .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
    // Upstream saw the real key, the endpoint's base path, no inbound headers.
    let seen = px.seen();
    assert_eq!(seen[0].headers["authorization"], format!("Bearer {KEY}"));
    assert!(!seen[0].headers.contains_key("x-leak"));
    assert_eq!(seen[0].headers["accept-encoding"], "identity");
    assert_eq!(seen[0].path, "/api/paas/v4/chat/completions");
    let recs = px.records().await;
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(
        (
            r.prompt_tokens,
            r.cached_tokens,
            r.prompt_cache_hit_tokens,
            r.completion_tokens,
            r.reasoning_tokens
        ),
        (Some(1000), Some(600), Some(600), Some(50), Some(7))
    );
    assert_eq!(r.n_tools, Some(2));
    assert!(!r.stream);
    assert!(!r.usage_missing);
    assert_eq!(r.resp_model.as_deref(), Some(MODEL));
    let want = (400.0 * 0.15 + 600.0 * 0.03 + 50.0 * 0.5) / 1e6;
    assert!((r.cost_usd.unwrap() - want).abs() < 1e-12);
    let raw: Value = serde_json::from_str(
        std::fs::read_to_string(&px.log)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    for k in [
        "ts",
        "path",
        "req_model",
        "status",
        "elapsed_ms",
        "ttfb_ms",
        "req_bytes",
        "usage_injected",
    ] {
        assert!(raw.get(k).is_some(), "record lacks {k}");
    }
}

#[tokio::test]
async fn streaming_split_and_last_chunk_usage_with_injection() {
    let px = fixture(Limits::default()).await;
    let (status, data, headers) = px
        .post(json!({"model": MODEL, "messages": msgs(), "stream": true}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/event-stream");
    let lines: Vec<&[u8]> = data
        .split(|&b| b == b'\n')
        .filter(|l| l.starts_with(b"data:"))
        .collect();
    assert_eq!(lines.last().unwrap(), b"data: [DONE]");
    assert_eq!(lines.len(), 5); // 3 deltas + usage chunk + DONE
    assert_eq!(
        px.seen()[0].body["stream_options"],
        json!({"include_usage": true})
    );
    let recs = px.records().await;
    let r = &recs[0];
    assert!(r.stream);
    assert_eq!(r.usage_injected, Some(true));
    assert_eq!(
        (r.prompt_tokens, r.cached_tokens, r.completion_tokens),
        (Some(2000), Some(1500), Some(120))
    );
    assert!(r.ttfb_ms.is_some());
}

#[tokio::test]
async fn streaming_client_stream_options_respected() {
    let px = fixture(Limits::default()).await;
    px.post(json!({"model": MODEL, "messages": msgs(), "stream": true,
                   "stream_options": {"include_usage": false}}))
        .await;
    assert_eq!(
        px.seen()[0].body["stream_options"],
        json!({"include_usage": false})
    );
    let r = &px.records().await[0];
    assert_eq!(r.usage_injected, Some(false));
    assert!(r.usage_missing);
    assert_eq!(r.prompt_tokens, None);
}

#[tokio::test]
async fn upstream_errors_pass_through() {
    let px = fixture(Limits::default()).await;
    for code in [429u16, 500] {
        let (status, data, _) = px
            .post(json!({"model": MODEL, "messages": msgs(), "user": format!("status-{code}")}))
            .await;
        assert_eq!(status, code);
        assert!(String::from_utf8_lossy(&data).contains(&format!("upstream {code}")));
    }
    let recs = px.records().await;
    assert_eq!(
        recs.iter().map(|r| r.status).collect::<Vec<_>>(),
        [429, 500]
    );
    assert!(recs.iter().all(|r| !r.usage_missing));
}

#[tokio::test]
async fn client_disconnect_mid_stream() {
    let px = fixture(Limits::default()).await;
    let port = px.port;
    tokio::task::spawn_blocking(move || {
        let mut s = StdTcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = json!({"model": MODEL, "messages": msgs(), "stream": true, "user": "slow"}).to_string();
        write!(
            s,
            "POST /chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("200"));
        // Dropped here: the client hangs up mid-stream.
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    // Proxy still serving, and the aborted request was logged.
    let (status, _, _) = px.post(json!({"model": MODEL, "messages": msgs()})).await;
    assert_eq!(status, 200);
    let recs = px.records().await;
    assert!(recs[0].client_aborted, "{:?}", recs[0]);
    assert!(recs[0].stream);
    assert!(!px.server.is_finished());
}

#[tokio::test]
async fn path_and_model_policy() {
    let px = fixture(Limits::default()).await;
    let (status, _, _) = px
        .post_path("/embeddings", json!({"model": MODEL, "messages": msgs()}))
        .await;
    assert_eq!(status, 404);
    let (status, data, _) = px.post(json!({"model": "gpt-9", "messages": msgs()})).await;
    assert_eq!(status, 403);
    let v: Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(v["error"]["type"], "model_rejected");
    assert!(px.seen().is_empty()); // neither reached upstream
    let recs = px.records().await;
    assert_eq!(
        recs.iter().map(|r| r.status).collect::<Vec<_>>(),
        [404, 403]
    );
    assert!(recs[1].model_rejected);
    // Model match is case-insensitive; /v1/chat/completions maps to the
    // same upstream path.
    let (status, _, _) = px
        .post_path(
            "/v1/chat/completions",
            json!({"model": "GLM-5.3-Flash", "messages": msgs()}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(px.seen()[0].path, "/api/paas/v4/chat/completions");
    assert_eq!(px.records().await[2].path, "/v1/chat/completions");
    // Health check is not logged.
    let h = reqwest::get(format!("http://127.0.0.1:{}/__meter/health", px.port))
        .await
        .unwrap();
    assert_eq!(h.status(), 200);
    assert_eq!(px.records().await.len(), 3);
}

#[tokio::test]
async fn log_has_no_secrets() {
    let px = fixture(Limits::default()).await;
    px.post(json!({"model": MODEL, "messages": msgs()})).await;
    px.post(json!({"model": MODEL, "messages": msgs(), "stream": true}))
        .await;
    px.post(json!({"model": MODEL, "messages": msgs(), "user": "status-500"}))
        .await;
    px.post(json!({"model": "other", "messages": msgs()})).await;
    px.records().await;
    let text = std::fs::read_to_string(&px.log).unwrap();
    assert_eq!(text.lines().count(), 4);
    for needle in [
        "Authorization",
        "authorization",
        "Bearer",
        "messages",
        "the-secret-prompt",
        KEY,
        "SECRET",
        "X-Leak",
        "\"hi\"",
        &format!("127.0.0.1:{}", px.upstream_port),
        "/api/paas",
    ] {
        assert!(!text.contains(needle), "log contains {needle}");
    }
}

#[tokio::test]
async fn budget_request_and_token_caps() {
    let px = fixture(Limits {
        max_requests: Some(3),
        max_tokens: Some(1500),
        max_cost_usd: None,
    })
    .await;
    let (s1, _, _) = px.post(json!({"model": MODEL, "messages": []})).await; // 1050 tokens
    let (s2, _, _) = px.post(json!({"model": MODEL, "messages": []})).await; // 2100 >= 1500 after this
    let (s3, d3, _) = px.post(json!({"model": MODEL, "messages": []})).await;
    assert_eq!((s1, s2, s3), (200, 200, 429));
    let v: Value = serde_json::from_slice(&d3).unwrap();
    assert_eq!(v["error"]["type"], "budget_exceeded");
    assert_eq!(px.seen().len(), 2);
    let recs = px.records().await;
    assert_eq!(recs[2].budget_exceeded.as_deref(), Some("max_tokens"));
}

#[tokio::test]
async fn budget_request_cap() {
    let px = fixture(Limits {
        max_requests: Some(1),
        ..Default::default()
    })
    .await;
    assert_eq!(px.post(json!({"model": MODEL})).await.0, 200);
    assert_eq!(px.post(json!({"model": MODEL})).await.0, 429);
    assert_eq!(
        px.records().await[1].budget_exceeded.as_deref(),
        Some("max_requests")
    );
}

#[tokio::test]
async fn ssrf_name_resolving_to_loopback_is_refused() {
    // Production policy (no test escape hatch): "localhost" passes URL
    // validation but the resolver refuses it at connect time.
    let seen: SeenLog = Arc::default();
    let upstream_port = fake_upstream(seen.clone()).await;
    let px = start(
        format!("https://localhost:{upstream_port}/v1"),
        false,
        Limits::default(),
        seen,
        upstream_port,
    )
    .await;
    let (status, data, _) = px.post(json!({"model": MODEL, "messages": msgs()})).await;
    assert_eq!(status, 502);
    assert!(String::from_utf8_lossy(&data).contains("meter policy"));
    assert!(px.seen().is_empty());
    let recs = px.records().await;
    assert_eq!(recs[0].upstream_error.as_deref(), Some("blocked_address"));
    let text = std::fs::read_to_string(&px.log).unwrap();
    assert!(!text.contains("localhost"));
}

/// The shipped binary has no way to allow a plain-http or loopback
/// upstream, takes the key on stdin only, and never echoes it.
#[test]
fn binary_refuses_insecure_upstream_and_hides_key() {
    let tmp = tempfile::tempdir().unwrap();
    for endpoint in [
        "http://127.0.0.1:9/v1",
        "https://127.0.0.1:9/v1",
        "https://[::1]/v1",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crucible-meter"))
            .args(["--port", "0", "--model", MODEL, "--log"])
            .arg(tmp.path().join("u.jsonl"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let cred = json!({"api_key": KEY, "endpoint": endpoint}).to_string();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{cred}\n").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{endpoint}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("SECRET"), "{err}");
    }
}

#[test]
fn binary_starts_with_public_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("u.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_crucible-meter"))
        .args([
            "--port",
            "0",
            "--model",
            MODEL,
            "--max-cost-usd",
            "1",
            "--log",
        ])
        .arg(&log)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let cred = json!({"api_key": KEY, "endpoint": "https://api.example.com/v1"}).to_string();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{cred}\n").as_bytes())
        .unwrap();
    // First stderr lines: the unknown-price warning (no --pricing) and the
    // listening address. No request is made: nothing leaves this machine.
    let mut stderr = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut text = String::new();
    for _ in 0..2 {
        std::io::BufRead::read_line(&mut stderr, &mut text).unwrap();
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(text.contains("meter: listening on 127.0.0.1:"), "{text}");
    assert!(text.contains("cost cap cannot be enforced"), "{text}");
    assert!(
        !text.contains("SECRET") && !text.contains("example.com"),
        "{text}"
    );
}
