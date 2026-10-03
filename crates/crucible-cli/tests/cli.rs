//! End-to-end checks of the `crucible` binary.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use crucible_crypto::PrivateKey;

fn crucible(args: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    if let Some(data) = stdin {
        pipe.write_all(data).unwrap();
    }
    drop(pipe);
    child.wait_with_output().unwrap()
}

fn pricing() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/pricing.json")
        .display()
        .to_string()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn seal_then_open_via_pipes() {
    let dir = tempfile::tempdir().unwrap();
    let sk = PrivateKey::generate();
    let key_file = dir.path().join("key.txt");
    std::fs::write(
        &key_file,
        format!("# test key\n{}\n", sk.to_secret_string()),
    )
    .unwrap();
    let plain = br#"{"api_key":"sk-SECRET","endpoint":"https://api.example.com/v1"}"#;
    let sealed = crucible(
        &["seal", "--recipient", &sk.public().to_string()],
        Some(plain),
    );
    assert!(sealed.status.success(), "{:?}", sealed);
    assert!(!sealed.stdout.windows(6).any(|w| w == b"SECRET"));
    let opened = crucible(&["open", "--identity", s(&key_file)], Some(&sealed.stdout));
    assert!(opened.status.success());
    assert_eq!(opened.stdout, plain);
    assert!(opened.stderr.is_empty());

    let other = dir.path().join("other.txt");
    std::fs::write(&other, PrivateKey::generate().to_secret_string()).unwrap();
    let wrong = crucible(&["open", "--identity", s(&other)], Some(&sealed.stdout));
    assert_eq!(wrong.status.code(), Some(2));
    assert!(wrong.stdout.is_empty());
}

#[test]
fn put_get_dir_store() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blob.bin");
    std::fs::write(&file, b"blob bytes").unwrap();
    let store = format!("dir:{}", s(&dir.path().join("store")));
    let put = crucible(&["put", "--store", &store, s(&file)], None);
    assert!(put.status.success(), "{put:?}");
    let hash = String::from_utf8(put.stdout).unwrap().trim().to_owned();
    assert_eq!(hash.len(), 64);
    let get = crucible(&["get", "--store", &store, &hash], None);
    assert_eq!(get.stdout, b"blob bytes");
    let bad = crucible(&["get", "--store", &store, "../x"], None);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn price_matches_prototype_cli() {
    // tools/tests/test_usage_report.py::test_cli
    let dir = tempfile::tempdir().unwrap();
    let usage = dir.path().join("usage.jsonl");
    std::fs::write(
        &usage,
        r#"{"resp_model":"glm-5","status":200,"prompt_tokens":1000000,"cached_tokens":0,"completion_tokens":1000000,"reasoning_tokens":0}"#.to_owned()
            + "\n\n",
    )
    .unwrap();
    let json = dir.path().join("s.json");
    let out = crucible(
        &[
            "price",
            "--usage",
            s(&usage),
            "--pricing",
            &pricing(),
            "--json",
            s(&json),
            "--markdown",
        ],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("$4.2000"));
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert!((v["total"]["cost_usd"].as_f64().unwrap() - 4.2).abs() < 1e-9);
}

#[test]
fn report_over_run_dir() {
    let dir = tempfile::tempdir().unwrap();
    for (rep, passed) in [("1", 10), ("2", 20)] {
        let st = dir.path().join(rep).join("stage-1");
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(
            st.join("result.json"),
            format!(r#"{{"status":"failed","passed":{passed},"total":30}}"#),
        )
        .unwrap();
    }
    let out = crucible(
        &[
            "report",
            "--run-dir",
            s(dir.path()),
            "--pricing",
            &pricing(),
        ],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["n_ok"], 2);
    assert_eq!(v["total"]["passed"]["mean"], 15.0);
}

#[test]
fn todo_commands_are_marked() {
    let help = crucible(&["--help"], None);
    let text = String::from_utf8_lossy(&help.stdout);
    for cmd in ["plan", "fetch", "build", "run", "package", "score"] {
        assert!(
            text.lines()
                .any(|l| l.trim_start().starts_with(cmd) && l.contains("TODO")),
            "{cmd} not marked TODO:\n{text}"
        );
        assert_eq!(crucible(&[cmd], None).status.code(), Some(2));
    }
}
