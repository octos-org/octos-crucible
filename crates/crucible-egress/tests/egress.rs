//! CONNECT allow / deny against a local echo target.

use std::path::{Path, PathBuf};

use crucible_egress::{EgressConfig, EgressRecord, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn echo_server() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

async fn proxy(cfg: EgressConfig) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { serve(l, cfg).await.unwrap() });
    port
}

fn config(log: &Path, port: u16, insecure: bool) -> EgressConfig {
    let mut c = EgressConfig::from_json(r#"{"allow_hosts": ["localhost"]}"#, log.into()).unwrap();
    c.port = port;
    c.insecure_allow_loopback_for_tests = insecure;
    c
}

async fn connect(proxy_port: u16, target: &str) -> (String, TcpStream) {
    let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
    s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = vec![0u8; 256];
    let n = s.read(&mut buf).await.unwrap();
    (String::from_utf8_lossy(&buf[..n]).into_owned(), s)
}

async fn records(log: &PathBuf) -> Vec<EgressRecord> {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[tokio::test]
async fn allowed_host_is_piped() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("egress.jsonl");
    let target = echo_server().await;
    let px = proxy(config(&log, target, true)).await;
    let (head, mut s) = connect(px, &format!("LocalHost:{target}")).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    s.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    drop(s);
    let recs = records(&log).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].host, "localhost");
    assert!(recs[0].allowed);
    assert_eq!((recs[0].up_bytes, recs[0].down_bytes), (Some(4), Some(4)));
}

#[tokio::test]
async fn denied_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("egress.jsonl");
    let target = echo_server().await;
    let px = proxy(config(&log, target, true)).await;
    // Not on the list; on the list but wrong port; not a CONNECT at all.
    for t in [
        format!("127.0.0.1:{target}"),
        "evil.example:443".into(),
        "localhost:80".into(),
    ] {
        let (head, _) = connect(px, &t).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{t}: {head}");
    }
    let mut s = TcpStream::connect(("127.0.0.1", px)).await.unwrap();
    s.write_all(b"GET http://localhost/ HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 64];
    let n = s.read(&mut buf).await.unwrap();
    assert!(buf[..n].starts_with(b"HTTP/1.1 403"));
    let recs = records(&log).await;
    assert_eq!(recs.len(), 4);
    assert!(recs.iter().all(|r| !r.allowed));
    assert_eq!(recs[1].host, "evil.example");
}

#[tokio::test]
async fn allowlisted_name_resolving_to_loopback_is_refused() {
    // Production policy: even an allowlisted name must resolve to public
    // addresses only.
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("egress.jsonl");
    let target = echo_server().await;
    let px = proxy(config(&log, target, false)).await;
    let (head, _) = connect(px, &format!("localhost:{target}")).await;
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    let recs = records(&log).await;
    assert!(!recs[0].allowed);
}
