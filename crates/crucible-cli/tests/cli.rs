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
fn score_is_still_todo() {
    let help = crucible(&["--help"], None);
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with("score") && l.contains("TODO"))
    );
    assert_eq!(crucible(&["score"], None).status.code(), Some(2));
    for cmd in [
        "plan", "fetch", "build", "run", "package", "taskset", "cred", "manifest",
    ] {
        assert!(
            text.lines()
                .any(|l| l.trim_start().starts_with(cmd) && !l.contains("TODO")),
            "{cmd} missing from help:\n{text}"
        );
    }
}

fn crucible_env(args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crucible"));
    cmd.args(args)
        .env_remove("GITHUB_OUTPUT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let mut pipe = child.stdin.take().unwrap();
    if let Some(data) = stdin {
        pipe.write_all(data).unwrap();
    }
    drop(pipe);
    child.wait_with_output().unwrap()
}

/// pack -> validate -> inputs (only inputs land) -> plan, end to end.
#[test]
fn taskset_pack_inputs_and_plan() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let src = root.join("src/stage-a");
    std::fs::create_dir_all(src.join("reference")).unwrap();
    std::fs::create_dir_all(src.join("tests/support")).unwrap();
    std::fs::write(src.join("requirements.yaml"), "id: ROOT").unwrap();
    std::fs::write(src.join("reference/a.png"), "png").unwrap();
    std::fs::write(src.join("tests/a.spec.ts"), "test('a')").unwrap();
    std::fs::write(src.join("tests/support/e2e.ts"), "x").unwrap();
    std::fs::write(
        root.join("source.json"),
        r#"{"schema":1,"name":"demo","scorer":{"name":"playwright"},"total_time_limit_s":600,
        "stages":[{"id":"stage-1","dir":"stage-a","inputs":["requirements.yaml","reference"],"tests":["tests"],"output":"web-app","time_limit_s":600,"expected_total":1}]}"#,
    )
    .unwrap();
    let sk = PrivateKey::generate();
    let pk = sk.public();
    std::fs::write(
        root.join("keys.json"),
        format!(
            r#"{{"current":"{id}","keys":[{{"key_id":"{id}","public_key":"{pk}"}}]}}"#,
            id = pk.key_id()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("repo/tasksets/demo")).unwrap();
    std::fs::create_dir_all(root.join("repo/agents/octos")).unwrap();
    std::fs::write(root.join("repo/agents/octos/agent.json"), "{}").unwrap();
    let ts = root.join("repo/tasksets/demo/taskset.json");
    let store = format!("dir:{}", root.join("store").display());
    let out = crucible(
        &[
            "taskset",
            "pack",
            "--source",
            s(&root.join("source.json")),
            "--src-dir",
            s(&root.join("src")),
            "--keys",
            s(&root.join("keys.json")),
            "--store",
            &store,
            "--out",
            s(&ts),
        ],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    let out = crucible(&["taskset", "validate", s(&ts)], None);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !crucible(
            &["taskset", "validate", s(&ts), "--max-total-s", "300"],
            None
        )
        .status
        .success()
    );

    let key_text = sk.to_secret_string();
    let out = crucible_env(
        &[
            "taskset",
            "inputs",
            "--taskset",
            s(&ts),
            "--store",
            &store,
            "--identity-env",
            "TEST_AGE_KEY",
            "--out",
            s(&root.join("inputs")),
        ],
        &[("TEST_AGE_KEY", &key_text)],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("AGE-SECRET"));
    assert!(root.join("inputs/stage-1/requirements.yaml").is_file());
    assert!(root.join("inputs/stage-1/reference/a.png").is_file());
    assert!(!root.join("inputs/stage-1/tests").exists());

    let gh_out = root.join("gh_output");
    let out = crucible_env(
        &[
            "plan",
            "--root",
            s(&root.join("repo")),
            "--github-output",
            s(&gh_out),
        ],
        &[
            ("IN_AGENT_SOURCE", "builtin:octos"),
            ("IN_TASKSET", "demo"),
            ("IN_MODEL", "glm-5.3-flash"),
            ("IN_REPLICAS", "2"),
            ("IN_CRED_SOURCE", "github-secret"),
            ("GITHUB_RUN_ID", "42"),
        ],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    let text = std::fs::read_to_string(&gh_out).unwrap();
    assert!(text.contains("matrix={\"replica\":[1,2]}\n"), "{text}");
    assert!(text.contains("eval_id=dev-42-1\n"));
    assert!(text.contains("run_timeout_min=18\n"));
    let bad = crucible_env(
        &["plan", "--root", s(&root.join("repo"))],
        &[
            ("IN_AGENT_SOURCE", "builtin:octos"),
            ("IN_TASKSET", "demo"),
            ("IN_MODEL", "x\ny=1"),
            ("IN_CRED_SOURCE", "github-secret"),
            ("GITHUB_RUN_ID", "42"),
        ],
        None,
    );
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn cred_open_from_env_pipes_one_line() {
    let sk = PrivateKey::generate();
    let plain = br#"{"api_key":"sk-SECRET","endpoint":"https://api.z.ai/api/coding/paas/v4"}"#;
    let sealed = crucible(
        &["seal", "--recipient", &sk.public().to_string()],
        Some(plain),
    );
    // Stored as a GitHub secret: base64 of the sealed bytes.
    let b64 = {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for ch in sealed.stdout.chunks(3) {
            let n = ch
                .iter()
                .enumerate()
                .fold(0u32, |a, (i, b)| a | (*b as u32) << (16 - 8 * i));
            for i in 0..=ch.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        s
    };
    let key = sk.to_secret_string();
    let out = crucible_env(
        &[
            "cred",
            "open",
            "--source",
            "github-secret",
            "--sealed-env",
            "SEALED",
            "--identity-env",
            "K",
        ],
        &[("SEALED", &b64), ("K", &key)],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    let line = String::from_utf8(out.stdout).unwrap();
    assert_eq!(line.matches('\n').count(), 1);
    assert!(line.contains("sk-SECRET"));
    assert!(out.stderr.is_empty());
    let kv = crucible_env(
        &[
            "cred",
            "open",
            "--source",
            "workers-kv",
            "--identity-env",
            "K",
        ],
        &[("K", &key)],
        None,
    );
    assert_eq!(kv.status.code(), Some(2));
    let wrong = crucible_env(
        &[
            "cred",
            "open",
            "--source",
            "github-secret",
            "--sealed-env",
            "SEALED",
            "--identity-env",
            "K",
        ],
        &[
            ("SEALED", &b64),
            ("K", &PrivateKey::generate().to_secret_string()),
        ],
        None,
    );
    assert_eq!(wrong.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&wrong.stderr).contains("SECRET"));
}

#[test]
fn package_web_app_zip() {
    let d = tempfile::tempdir().unwrap();
    let w = d.path().join("work");
    std::fs::create_dir_all(w.join("backend")).unwrap();
    std::fs::create_dir_all(w.join("frontend/node_modules/x")).unwrap();
    std::fs::write(w.join("backend/server.js"), "x").unwrap();
    std::fs::write(w.join("frontend/node_modules/x/i.js"), "x").unwrap();
    let zip = d.path().join("app.zip");
    let out = crucible(
        &[
            "package",
            "--kind",
            "web-app",
            "--src",
            s(&w),
            "--out",
            s(&zip),
        ],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    let data = std::fs::read(&zip).unwrap();
    assert!(data.windows(10).any(|w| w == b"Dockerfile"));
    assert!(!data.windows(12).any(|w| w == b"node_modules"));
    std::fs::remove_dir_all(w.join("backend")).unwrap();
    assert_eq!(
        crucible(&["package", "--src", s(&w), "--out", s(&zip)], None)
            .status
            .code(),
        Some(2)
    );
}
