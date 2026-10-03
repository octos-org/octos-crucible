//! The HTTP side: accept, police, forward, record.

use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use crucible_core::UsageRecord;
use crucible_metering::{Price, Pricing, cost_usd, extract_usage};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::budget::Budget;
use crate::upstream::{Upstream, build_client, classify};
use crate::{ConfigError, MeterConfig, now_ts, reject_record, truncate};

pub const HEALTH_PATH: &str = "/__meter/health";
/// `/v1/...` is accepted because many SDKs append it to a bare host; both
/// are forwarded to `<endpoint>/chat/completions`.
const ALLOWED_PATHS: [&str; 2] = ["/chat/completions", "/v1/chat/completions"];
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// One SSE line longer than this is relayed but not parsed (usage lines are
/// small; this only bounds memory).
const MAX_SSE_LINE: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = concat!("crucible-meter/", env!("CARGO_PKG_VERSION"));

type ProxyBody = BoxBody<Bytes, io::Error>;

struct Meter {
    upstream: Upstream,
    client: reqwest::Client,
    model: String,
    pricing: Pricing,
    user_price: Option<Price>,
    force_usage: bool,
    budget: Budget,
    log: Mutex<File>,
}

impl MeterConfig {
    /// A cost cap needs a price; without one only request/token caps apply.
    pub fn cost_cap_enforceable(&self) -> bool {
        self.user_price.is_some() || self.pricing.lookup(Some(&self.model)).is_some()
    }
}

/// Serve on `listener` until the task is dropped.
pub async fn serve(listener: TcpListener, cfg: MeterConfig) -> Result<(), ConfigError> {
    if cfg.limits.max_cost_usd.is_some() && !cfg.cost_cap_enforceable() {
        eprintln!(
            "meter: no known price for this model; the cost cap cannot be enforced, \
             request/token caps still apply"
        );
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.log_path)
        .map_err(|e| ConfigError::Invalid(format!("usage log: {e}")))?;
    let meter = Arc::new(Meter {
        client: build_client(cfg.insecure_allow_loopback_for_tests)?,
        upstream: cfg.upstream,
        model: cfg.model.to_lowercase(),
        pricing: cfg.pricing,
        user_price: cfg.user_price,
        force_usage: cfg.force_usage,
        budget: Budget::new(cfg.limits),
        log: Mutex::new(log),
    });
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            // Transient (EMFILE, ECONNABORTED): keep serving.
            Err(_) => continue,
        };
        let meter = meter.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| handle(meter.clone(), req));
            // A client hanging up mid-request is normal for agents (cancel,
            // kill); the connection error itself carries nothing to log.
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await;
        });
    }
}

async fn handle(
    meter: Arc<Meter>,
    req: Request<Incoming>,
) -> Result<Response<ProxyBody>, Infallible> {
    let t0 = Instant::now();
    let path = req.uri().path().to_owned();
    if req.method() == Method::GET && path == HEALTH_PATH {
        return Ok(json_response(StatusCode::OK, &json!({"ok": true})));
    }
    let mut rec = UsageRecord {
        path: truncate(&path, 200),
        ..Default::default()
    };
    if req.method() != Method::POST || !ALLOWED_PATHS.contains(&path.as_str()) {
        return Ok(meter.reject(rec, StatusCode::NOT_FOUND, "not_found", "not found"));
    }
    let body = match Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
    {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return Ok(meter.reject(
                rec,
                StatusCode::BAD_REQUEST,
                "bad_request",
                "body too large or unreadable",
            ));
        }
    };
    rec.req_bytes = Some(body.len() as u64);
    let Ok(Value::Object(mut payload)) = serde_json::from_slice::<Value>(&body) else {
        return Ok(meter.reject(
            rec,
            StatusCode::BAD_REQUEST,
            "bad_request",
            "body is not a JSON object",
        ));
    };
    rec.req_model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(|m| truncate(m, 200));
    rec.stream = truthy(payload.get("stream"));
    rec.n_tools = Some(
        payload
            .get("tools")
            .and_then(Value::as_array)
            .map_or(0, |t| t.len() as u64),
    );
    if rec.req_model.as_deref().map(str::to_lowercase).as_deref() != Some(meter.model.as_str()) {
        rec.model_rejected = true;
        return Ok(meter.reject(
            rec,
            StatusCode::FORBIDDEN,
            "model_rejected",
            "model not allowed for this run",
        ));
    }
    if let Some(cap) = meter.budget.reserve() {
        rec.budget_exceeded = Some(cap.into());
        let msg = format!("meter budget exceeded ({cap})");
        return Ok(meter.reject(rec, StatusCode::TOO_MANY_REQUESTS, "budget_exceeded", &msg));
    }
    rec.usage_injected = Some(false);
    let body = if rec.stream && !payload.contains_key("stream_options") && meter.force_usage {
        let mut opts = Map::new();
        opts.insert("include_usage".into(), Value::Bool(true));
        payload.insert("stream_options".into(), Value::Object(opts));
        rec.usage_injected = Some(true);
        Bytes::from(serde_json::to_vec(&payload).expect("JSON object serializes"))
    } else {
        body
    };
    Ok(forward(meter, body, rec, t0).await)
}

async fn forward(
    meter: Arc<Meter>,
    body: Bytes,
    mut rec: UsageRecord,
    t0: Instant,
) -> Response<ProxyBody> {
    let accept = if rec.stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let sent = meter
        .client
        .post(meter.upstream.chat_url().clone())
        .header("Content-Type", "application/json")
        .header("Accept", accept)
        .header("Accept-Encoding", "identity")
        .header("User-Agent", USER_AGENT)
        .header("Authorization", meter.upstream.bearer())
        .body(body)
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => {
            let class = classify(&e);
            rec.upstream_error = Some(class.into());
            let msg = if class == "blocked_address" {
                "upstream refused by meter policy"
            } else {
                "upstream unreachable"
            };
            meter.finish(rec, 502, None, None, t0, None);
            return error_response(StatusCode::BAD_GATEWAY, "upstream_error", msg);
        }
    };
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
    if ctype.as_bytes().starts_with(b"text/event-stream") {
        return relay_stream(meter, resp, rec, t0, status, ctype);
    }

    let data = match resp.bytes().await {
        Ok(d) => d,
        Err(e) => {
            rec.upstream_error = Some(classify(&e).into());
            meter.finish(rec, 502, None, None, t0, None);
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "upstream unreachable",
            );
        }
    };
    let ttfb = Instant::now();
    let (mut usage, mut resp_model) = (None, None);
    if status == StatusCode::OK
        && let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(&data)
    {
        usage = obj.remove("usage");
        resp_model = obj.get("model").and_then(Value::as_str).map(String::from);
    }
    meter.finish(rec, status.as_u16(), usage, resp_model, t0, Some(ttfb));
    let mut out = Response::new(Full::new(data).map_err(|e| match e {}).boxed());
    *out.status_mut() = status;
    out.headers_mut().insert(CONTENT_TYPE, ctype);
    out
}

/// Relay an SSE response chunk by chunk as upstream produces it; the record
/// is written when the stream ends, the upstream fails, or the client leaves.
fn relay_stream(
    meter: Arc<Meter>,
    mut resp: reqwest::Response,
    mut rec: UsageRecord,
    t0: Instant,
    status: StatusCode,
    ctype: HeaderValue,
) -> Response<ProxyBody> {
    // Small buffer: backpressure from a slow client reaches upstream, and a
    // vanished client is noticed within a few chunks.
    let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(8);
    tokio::spawn(async move {
        let mut scan = SseScan::default();
        let mut ttfb = None;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    ttfb.get_or_insert_with(Instant::now);
                    scan.feed(&chunk);
                    if tx.send(Ok(chunk)).await.is_err() {
                        rec.client_aborted = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    rec.upstream_error = Some(classify(&e).into());
                    // Make the client see a broken stream, not a clean end.
                    let _ = tx
                        .send(Err(io::Error::other("upstream stream interrupted")))
                        .await;
                    break;
                }
            }
        }
        scan.finish();
        meter.finish(rec, status.as_u16(), scan.usage, scan.model, t0, ttfb);
    });
    let mut out = Response::new(ChannelBody(rx).boxed());
    *out.status_mut() = status;
    out.headers_mut().insert(CONTENT_TYPE, ctype);
    out.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    out
}

impl Meter {
    fn reject(
        &self,
        rec: UsageRecord,
        status: StatusCode,
        kind: &str,
        msg: &str,
    ) -> Response<ProxyBody> {
        self.write(reject_record(rec, status.as_u16()));
        error_response(status, kind, msg)
    }

    fn finish(
        &self,
        mut rec: UsageRecord,
        status: u16,
        usage: Option<Value>,
        resp_model: Option<String>,
        t0: Instant,
        ttfb: Option<Instant>,
    ) {
        let counts = usage.as_ref().and_then(extract_usage);
        rec.status = status;
        rec.resp_model = resp_model;
        rec.elapsed_ms = t0.elapsed().as_millis() as u64;
        rec.ttfb_ms = ttfb.map(|t| t.duration_since(t0).as_millis() as u64);
        rec.usage_missing = status == 200 && counts.is_none();
        if let Some(c) = counts {
            let price = self
                .user_price
                .or_else(|| self.pricing.lookup(rec.billed_model()));
            let cost = cost_usd(price, c.prompt_tokens, c.cached_tokens, c.completion_tokens);
            self.budget.add(c.prompt_tokens + c.completion_tokens, cost);
            rec.prompt_tokens = Some(c.prompt_tokens);
            rec.cached_tokens = Some(c.cached_tokens);
            rec.prompt_cache_hit_tokens = Some(c.cached_tokens);
            rec.completion_tokens = Some(c.completion_tokens);
            rec.reasoning_tokens = Some(c.reasoning_tokens);
            rec.cost_usd = cost.map(|c| (c * 1e8).round() / 1e8);
        }
        self.write(rec);
    }

    fn write(&self, mut rec: UsageRecord) {
        rec.ts = now_ts();
        let mut line = serde_json::to_vec(&rec).expect("record serializes");
        line.push(b'\n');
        let mut f = self.log.lock().unwrap();
        // One write per record keeps lines whole under O_APPEND. A failed
        // write must not take the proxy down mid-run.
        if f.write_all(&line).is_err() {
            eprintln!("meter: could not write usage record");
        }
    }
}

/// Collects `data:` lines across arbitrary chunk boundaries and keeps the
/// last usage object and model name seen.
#[derive(Default)]
struct SseScan {
    buf: Vec<u8>,
    overflow: bool,
    usage: Option<Value>,
    model: Option<String>,
}

impl SseScan {
    fn feed(&mut self, chunk: &[u8]) {
        let mut rest = chunk;
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            if !self.overflow {
                self.buf.extend_from_slice(&rest[..i]);
                let line = std::mem::take(&mut self.buf);
                self.line(&line);
            }
            self.buf.clear();
            self.overflow = false;
            rest = &rest[i + 1..];
        }
        if !self.overflow {
            self.buf.extend_from_slice(rest);
            if self.buf.len() > MAX_SSE_LINE {
                self.buf = Vec::new();
                self.overflow = true;
            }
        }
    }

    fn finish(&mut self) {
        if !self.overflow && !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.line(&line);
        }
    }

    fn line(&mut self, line: &[u8]) {
        let Some(data) = line.trim_ascii().strip_prefix(b"data:") else {
            return;
        };
        let data = data.trim_ascii();
        if data.is_empty() || data == b"[DONE]" {
            return;
        }
        let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(data) else {
            return;
        };
        if let Some(u) = obj.remove("usage").filter(truthy_value) {
            self.usage = Some(u);
        }
        if let Some(m) = obj
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            self.model = Some(m.to_owned());
        }
    }
}

/// Python truthiness, as the prototype applied to `stream` and `usage`.
fn truthy(v: Option<&Value>) -> bool {
    v.is_some_and(truthy_value)
}

fn truthy_value(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn json_response(status: StatusCode, v: &Value) -> Response<ProxyBody> {
    let data = Bytes::from(serde_json::to_vec(v).expect("JSON serializes"));
    let mut out = Response::new(Full::new(data).map_err(|e| match e {}).boxed());
    *out.status_mut() = status;
    out.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    out
}

fn error_response(status: StatusCode, kind: &str, msg: &str) -> Response<ProxyBody> {
    json_response(status, &json!({"error": {"type": kind, "message": msg}}))
}

/// Response body fed by the relay task. When the client goes away hyper
/// drops this body, the receiver closes, and the relay task's next send
/// fails: that is how a client abort is detected.
struct ChannelBody(mpsc::Receiver<io::Result<Bytes>>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.0.poll_recv(cx).map(|o| o.map(|r| r.map(Frame::data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_split_anywhere() {
        let raw = b"data: {\"model\":\"m\",\"usage\":null}\n\n: comment\ndata: {\"model\":\"m2\",\"choices\":[],\"usage\":{\"prompt_tokens\":5}}\r\n\r\ndata: [DONE]\n\n";
        for step in [1, 3, 7, 37, raw.len()] {
            let mut s = SseScan::default();
            for c in raw.chunks(step) {
                s.feed(c);
            }
            s.finish();
            assert_eq!(s.model.as_deref(), Some("m2"), "step {step}");
            assert_eq!(s.usage.as_ref().unwrap()["prompt_tokens"], 5, "step {step}");
        }
    }

    #[test]
    fn sse_last_usage_wins_and_empty_ignored() {
        let mut s = SseScan::default();
        s.feed(b"data: {\"usage\":{\"prompt_tokens\":1}}\ndata: {\"usage\":{}}\ndata: {\"usage\":{\"prompt_tokens\":2}}");
        s.finish(); // last line has no newline
        assert_eq!(s.usage.unwrap()["prompt_tokens"], 2);
    }

    #[test]
    fn sse_overlong_line_skipped() {
        let mut s = SseScan::default();
        s.feed(b"data: {\"usage\":{\"prompt_tokens\":1}}\ndata: ");
        s.feed(&vec![b'x'; MAX_SSE_LINE + 1]);
        s.feed(b"\ndata: {\"model\":\"after\"}\n");
        assert_eq!(s.model.as_deref(), Some("after"));
        assert_eq!(s.usage.unwrap()["prompt_tokens"], 1);
    }

    #[test]
    fn python_truthiness() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&json!(false))));
        assert!(!truthy(Some(&json!(0))));
        assert!(truthy(Some(&json!(true))));
        assert!(truthy(Some(&json!(1))));
        assert!(!truthy(Some(&json!({}))));
    }
}
