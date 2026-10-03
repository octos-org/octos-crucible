//! Metering proxy between an agent container and the user's model endpoint.
//!
//! The agent only ever sees this proxy's address and a dummy key. The proxy:
//! - accepts `POST /chat/completions` (and `/v1/chat/completions`) only;
//!   anything else is 404 and logged;
//! - allows only the run's model (403 `model_rejected` otherwise);
//! - optionally enforces request/token/cost caps (429 `budget_exceeded`,
//!   upstream not called); no caps by default;
//! - drops every inbound header and sends upstream only Content-Type,
//!   Accept, Accept-Encoding: identity, User-Agent and the real key;
//! - with `force_usage`, adds `stream_options.include_usage` to streaming
//!   requests that set no `stream_options` (record: `usage_injected`);
//! - relays SSE chunk by chunk as it arrives and reads usage from the
//!   `data:` lines (last usage wins); non-streaming responses are read whole;
//! - writes one JSONL [`UsageRecord`] per request. Never logged: headers,
//!   messages, response bodies, the key, the upstream endpoint.
//!
//! The real credential arrives on stdin (one JSON line), never via argv,
//! env or disk. Behaviour follows the prototype `tools/meter_proxy.py`.

mod budget;
mod proxy;
mod upstream;

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;

use crucible_core::UsageRecord;
use crucible_core::netpolicy::EndpointError;
use crucible_metering::{Price, Pricing};

pub use budget::Limits;
pub use proxy::{HEALTH_PATH, serve};
pub use upstream::{Credential, Upstream};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error("no upstream API key")]
    NoKey,
    #[error("credential on stdin must be one JSON line {{\"api_key\", \"endpoint\"}}")]
    Credential,
    #[error("could not build the HTTP client")]
    Client,
    #[error("{0}")]
    Invalid(String),
}

pub struct MeterConfig {
    pub upstream: Upstream,
    /// The only model this run may request (case-insensitive).
    pub model: String,
    pub pricing: Pricing,
    /// User-supplied price; wins over `pricing`.
    pub user_price: Option<Price>,
    pub force_usage: bool,
    pub limits: Limits,
    pub log_path: PathBuf,
    /// Allows http and loopback/private upstreams so tests can run against
    /// a local fake. Must stay false in production; the CLI cannot set it.
    pub insecure_allow_loopback_for_tests: bool,
}

/// Read the credential: exactly one line of JSON. Only the first line is
/// consumed so nothing else on stdin is ever buffered with the key.
pub fn read_credential(input: &mut impl BufRead) -> Result<Credential, ConfigError> {
    let mut line = String::new();
    input
        .read_line(&mut line)
        .map_err(|_| ConfigError::Credential)?;
    let cred = serde_json::from_str(line.trim()).map_err(|_| ConfigError::Credential);
    // Best effort: do not leave the plaintext in this buffer.
    line.clear();
    line.shrink_to_fit();
    cred
}

/// `crucible meter` / `crucible-meter` arguments.
#[derive(Debug, clap::Args)]
pub struct MeterArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    #[arg(long, default_value_t = 8787)]
    pub port: u16,
    /// usage JSONL to append to.
    #[arg(long)]
    pub log: PathBuf,
    /// The only model the agent may request.
    #[arg(long)]
    pub model: String,
    /// Price table (config/pricing.json).
    #[arg(long)]
    pub pricing: Option<PathBuf>,
    /// User price {"input","cached_input","output"}, USD per 1M tokens.
    #[arg(long)]
    pub price_json: Option<String>,
    /// Add stream_options.include_usage to streaming requests that lack it.
    #[arg(long)]
    pub force_usage: bool,
    #[arg(long)]
    pub max_requests: Option<u64>,
    #[arg(long)]
    pub max_tokens: Option<u64>,
    /// Enforced only when the model's price is known.
    #[arg(long)]
    pub max_cost_usd: Option<f64>,
}

impl MeterArgs {
    pub fn into_config(self, cred: Credential) -> Result<MeterConfig, ConfigError> {
        let pricing = match &self.pricing {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .map_err(|e| ConfigError::Invalid(format!("pricing: {e}")))?;
                Pricing::from_json(&raw).map_err(|e| ConfigError::Invalid(e.to_string()))?
            }
            None => Pricing::default(),
        };
        let user_price = match &self.price_json {
            Some(raw) => {
                let v: serde_json::Value = serde_json::from_str(raw)
                    .map_err(|_| ConfigError::Invalid("--price-json is not JSON".into()))?;
                Some(
                    crucible_metering::parse_price(&v)
                        .map_err(|e| ConfigError::Invalid(e.to_string()))?
                        .ok_or_else(|| {
                            ConfigError::Invalid("--price-json needs input and output".into())
                        })?,
                )
            }
            None => None,
        };
        if self.max_requests == Some(0)
            || self.max_tokens == Some(0)
            || self.max_cost_usd.is_some_and(|c| c.is_nan() || c <= 0.0)
        {
            return Err(ConfigError::Invalid(
                "caps must be > 0 (omit a cap for no limit)".into(),
            ));
        }
        if self.model.trim().is_empty() {
            return Err(ConfigError::Invalid("--model is empty".into()));
        }
        Ok(MeterConfig {
            upstream: Upstream::new(cred, false)?,
            model: self.model,
            pricing,
            user_price,
            force_usage: self.force_usage,
            limits: Limits {
                max_requests: self.max_requests,
                max_tokens: self.max_tokens,
                max_cost_usd: self.max_cost_usd,
            },
            log_path: self.log,
            insecure_allow_loopback_for_tests: false,
        })
    }
}

/// Read the credential from stdin, then serve until SIGINT/SIGTERM.
pub async fn run(args: MeterArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cred = read_credential(&mut std::io::stdin().lock())?;
    let addr: SocketAddr = format!("{}:{}", args.bind, args.port)
        .parse()
        .map_err(|_| ConfigError::Invalid("--bind/--port is not a socket address".into()))?;
    let cfg = args.into_config(cred)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("meter: listening on {}", listener.local_addr()?);
    tokio::select! {
        r = serve(listener, cfg) => r?,
        _ = shutdown_signal() => {}
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Timestamp in the prototype's format, `YYYY-MM-DDTHH:MM:SSZ`.
fn now_ts() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Shared by the proxy and tests: the record a rejected request leaves.
fn reject_record(mut rec: UsageRecord, status: u16) -> UsageRecord {
    rec.status = status;
    rec.elapsed_ms = 0;
    rec.usage_missing = false;
    rec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_one_line() {
        let mut input: &[u8] =
            b"{\"api_key\":\"sk\",\"endpoint\":\"https://api.example.com/v1\"}\nrest";
        let c = read_credential(&mut input).unwrap();
        assert_eq!(c.api_key, "sk");
        assert_eq!(input, b"rest");
        let mut bad: &[u8] = b"api_key=sk\n";
        assert!(matches!(
            read_credential(&mut bad),
            Err(ConfigError::Credential)
        ));
    }

    #[test]
    fn ts_format() {
        let ts = now_ts();
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z') && ts.as_bytes()[10] == b'T');
    }
}
