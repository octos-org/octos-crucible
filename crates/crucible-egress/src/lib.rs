//! Allowlisting HTTPS egress proxy for the agent container.
//!
//! Speaks plain HTTP `CONNECT`; accepts only `CONNECT <host>:443` for hosts
//! listed in `config/egress.json`, resolves the host, refuses it unless every
//! address is public, connects to the checked address and then just pipes
//! bytes (TLS stays end to end; nothing is inspected). Everything else gets
//! 403. One JSONL record per connection: host, port, allowed, byte counts.
//! Behaviour follows the prototype `tools/egress_proxy.py`.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_core::netpolicy::is_public_ip;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HEAD_LINE: usize = 4096;
const MAX_HEAD_LINES: usize = 100;

/// `config/egress.json`.
#[derive(Debug, Deserialize)]
pub struct AllowList {
    /// Exact host names, no wildcards.
    pub allow_hosts: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("egress config: {0}")]
    Config(String),
    #[error("egress log: {0}")]
    Log(std::io::Error),
}

pub struct EgressConfig {
    pub allow_hosts: HashSet<String>,
    /// 443 in production; tests point it at a local echo server.
    pub port: u16,
    pub log_path: PathBuf,
    /// Allows allowlisted hosts that resolve to loopback/private addresses,
    /// so tests can use a local target. The CLI cannot set it.
    pub insecure_allow_loopback_for_tests: bool,
}

impl EgressConfig {
    pub fn from_json(raw: &str, log_path: PathBuf) -> Result<Self, EgressError> {
        let list: AllowList =
            serde_json::from_str(raw).map_err(|e| EgressError::Config(e.to_string()))?;
        Ok(EgressConfig {
            allow_hosts: list
                .allow_hosts
                .iter()
                .map(|h| h.trim().to_lowercase())
                .filter(|h| !h.is_empty())
                .collect(),
            port: 443,
            log_path,
            insecure_allow_loopback_for_tests: false,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressRecord {
    pub ts: String,
    pub host: String,
    pub port: u16,
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Egress {
    cfg: EgressConfig,
    log: Mutex<File>,
}

pub async fn serve(listener: TcpListener, cfg: EgressConfig) -> Result<(), EgressError> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&cfg.log_path)
        .map_err(EgressError::Log)?;
    let egress = Arc::new(Egress {
        cfg,
        log: Mutex::new(log),
    });
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            continue;
        };
        let egress = egress.clone();
        tokio::spawn(async move { egress.handle(sock).await });
    }
}

impl Egress {
    async fn handle(&self, sock: TcpStream) {
        let mut reader = BufReader::new(sock);
        let Ok(Some((host, port))) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut reader))
            .await
            .unwrap_or(Ok(None))
        else {
            // Not even a parseable request: log as a refusal of nothing.
            self.refuse(reader, EgressRecord::default()).await;
            return;
        };
        let mut rec = EgressRecord {
            host: host.chars().take(200).collect(),
            port,
            ..Default::default()
        };
        let Some(addrs) = self.allowed(&host, port).await else {
            self.refuse(reader, rec).await;
            return;
        };
        rec.allowed = true;
        let upstream = match connect_any(&addrs).await {
            Some(s) => s,
            None => {
                rec.error = Some("connect_failed".into());
                self.write(rec);
                let _ = reader
                    .get_mut()
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            }
        };
        // Bytes the client sent after the head (e.g. an eager TLS hello)
        // are already in our buffer; they go upstream first.
        let early = reader.buffer().to_vec();
        let mut client = reader.into_inner();
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let (up, down) = pipe(client, upstream, early).await;
        rec.up_bytes = Some(up);
        rec.down_bytes = Some(down);
        self.write(rec);
    }

    /// The checked addresses to connect to, if the request is allowed.
    async fn allowed(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        if port != self.cfg.port || !self.cfg.allow_hosts.contains(host) {
            return None;
        }
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.ok()?.collect();
        if !self.cfg.insecure_allow_loopback_for_tests
            && addrs.iter().any(|a| !is_public_ip(a.ip()))
        {
            return None;
        }
        (!addrs.is_empty()).then_some(addrs)
    }

    async fn refuse(&self, mut reader: BufReader<TcpStream>, rec: EgressRecord) {
        self.write(rec);
        let _ = reader
            .get_mut()
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await;
    }

    fn write(&self, mut rec: EgressRecord) {
        rec.ts = humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string();
        let mut line = serde_json::to_vec(&rec).expect("record serializes");
        line.push(b'\n');
        if self.log.lock().unwrap().write_all(&line).is_err() {
            eprintln!("egress: could not write log record");
        }
    }
}

/// Connect to the first checked address that answers (never re-resolving,
/// so the address that passed the check is the one used).
async fn connect_any(addrs: &[SocketAddr]) -> Option<TcpStream> {
    for addr in addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            return Some(s);
        }
    }
    None
}

/// Parse `CONNECT host:port HTTP/1.x` and skip the remaining header lines.
/// `Ok(None)` for anything else. Host is lower-cased, IPv6 brackets removed.
async fn read_head(r: &mut BufReader<TcpStream>) -> std::io::Result<Option<(String, u16)>> {
    let first = read_line(r).await?;
    for _ in 0..MAX_HEAD_LINES {
        let line = read_line(r).await?;
        if line.is_empty() {
            break;
        }
    }
    Ok(parse_connect(&first))
}

async fn read_line(r: &mut BufReader<TcpStream>) -> std::io::Result<String> {
    let mut buf = Vec::new();
    (&mut *r)
        .take(MAX_HEAD_LINE as u64)
        .read_until(b'\n', &mut buf)
        .await?;
    Ok(String::from_utf8_lossy(&buf).trim().to_owned())
}

fn parse_connect(line: &str) -> Option<(String, u16)> {
    let mut parts = line.split_whitespace();
    let (Some("CONNECT"), Some(target), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let (host, port) = target.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port: u16 = port.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_lowercase(), port))
}

/// Copy both ways until either side closes or nothing moves for
/// `IDLE_TIMEOUT`. Returns (client→upstream, upstream→client) byte counts.
async fn pipe(client: TcpStream, upstream: TcpStream, early: Vec<u8>) -> (u64, u64) {
    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();
    let (mut up, mut down) = (0u64, 0u64);
    if !early.is_empty() {
        if uw.write_all(&early).await.is_err() {
            return (0, 0);
        }
        up += early.len() as u64;
    }
    let mut a = vec![0u8; 64 * 1024];
    let mut b = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            r = cr.read(&mut a) => match r {
                Ok(n) if n > 0 => {
                    if uw.write_all(&a[..n]).await.is_err() { break; }
                    up += n as u64;
                }
                _ => break,
            },
            r = ur.read(&mut b) => match r {
                Ok(n) if n > 0 => {
                    if cw.write_all(&b[..n]).await.is_err() { break; }
                    down += n as u64;
                }
                _ => break,
            },
            _ = tokio::time::sleep(IDLE_TIMEOUT) => break,
        }
    }
    (up, down)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_line() {
        assert_eq!(
            parse_connect("CONNECT PyPI.org:443 HTTP/1.1"),
            Some(("pypi.org".into(), 443))
        );
        assert_eq!(
            parse_connect("CONNECT [::1]:443 HTTP/1.1"),
            Some(("::1".into(), 443))
        );
        assert_eq!(parse_connect("GET http://pypi.org/ HTTP/1.1"), None);
        assert_eq!(parse_connect("CONNECT pypi.org HTTP/1.1"), None);
        assert_eq!(parse_connect("CONNECT pypi.org:x HTTP/1.1"), None);
        assert_eq!(parse_connect("CONNECT :443 HTTP/1.1"), None);
        assert_eq!(parse_connect(""), None);
    }

    #[test]
    fn config() {
        let c = EgressConfig::from_json(
            r#"{"allow_hosts": ["Registry.npmjs.org", " pypi.org ", ""]}"#,
            "x".into(),
        )
        .unwrap();
        assert!(c.allow_hosts.contains("registry.npmjs.org"));
        assert!(c.allow_hosts.contains("pypi.org"));
        assert_eq!(c.allow_hosts.len(), 2);
        assert_eq!(c.port, 443);
        assert!(!c.insecure_allow_loopback_for_tests);
    }
}
