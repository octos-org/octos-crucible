//! End-to-end flow through the real router with D1 as in-memory SQLite (the
//! real migration) and a fake GitHub: login → upload → submit → Actions fetches and deletes the
//! credential → results → query → download, plus the refusal paths.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use crucible_core::Envelope;
use crucible_worker::app::handle;
use crucible_worker::config::Config;
use crucible_worker::http::{Backend, HttpRequest, HttpResponse, Req, Resp, Row, SqlArg, Stmt};
use crucible_worker::session::issue_session;
use crucible_worker::shard::{release_tag, sha256_hex};
use crucible_worker::store::Db;
use crucible_worker::util::b64_encode;
use crucible_worker::{keys, util};
use serde_json::{Value, json};

const SESSION_KEY: &str = "test-session-key-test-session-key-0001";
const WORKER_TOKEN: &str = "test-worker-token-test-worker-token-0001";
const GH_TOKEN: &str = "ghp_platform_token";
const CLIENT_SECRET: &str = "oauth-client-secret";
const PAGES: &str = "https://octos-org.github.io";
const EID: &str = "0b7e6a52-1f3c-4d2a-9e8b-7c6d5e4f3a21";
const NOW: u64 = 1_790_985_600;

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("mock backend never pends"),
    }
}

#[derive(Default)]
struct FakeGitHub {
    releases: BTreeMap<String, u64>,
    /// release id → asset name → bytes
    assets: BTreeMap<u64, BTreeMap<String, Vec<u8>>>,
    /// The next asset upload is stored but its answer is lost.
    lose_upload_answer: bool,
    dispatches: Vec<Value>,
    /// (run id, eval id, status, conclusion)
    runs: Vec<(u64, String, String, Option<String>)>,
}

const SCHEMA: &str = concat!(
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_leaderboard.sql"),
    include_str!("../migrations/0003_user_plugins.sql"),
    include_str!("../migrations/0004_creds.sql"),
    include_str!("../migrations/0005_plugin_review.sql"),
    include_str!("../migrations/0006_quotas.sql")
);

struct Mock {
    /// D1 stand-in: SQLite in memory with the real migration applied.
    db: rusqlite::Connection,
    gh: RefCell<FakeGitHub>,
    now: Cell<u64>,
    logs: RefCell<Vec<String>>,
    rng: Cell<u8>,
    /// When set, every D1 write fails with this error.
    write_error: RefCell<Option<String>>,
    /// D1 rows changed by successful writes outside the `cache` table.
    db_writes: Cell<usize>,
}

impl Mock {
    fn new() -> Mock {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(SCHEMA).unwrap();
        Mock {
            db,
            gh: RefCell::default(),
            now: Cell::new(NOW),
            logs: RefCell::default(),
            rng: Cell::new(7),
            write_error: RefCell::default(),
            db_writes: Cell::new(0),
        }
    }

    /// D1 rows written so far (outside `cache`).
    fn writes(&self) -> usize {
        self.db_writes.get()
    }

    /// Test-side SQL (setup and inspection); not counted.
    fn sql(&self, sql: &str) {
        self.db.execute_batch(sql).unwrap();
    }

    fn count(&self, sql: &str) -> i64 {
        self.db.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn write_failure(&self) -> Result<(), String> {
        match self.write_error.borrow().clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn exec(&self, sql: &str, args: &[SqlArg]) -> Result<u64, String> {
        let mut stmt = self.db.prepare(sql).map_err(|e| e.to_string())?;
        let n = stmt
            .execute(rusqlite::params_from_iter(args.iter().map(to_sqlite)))
            .map_err(|e| e.to_string())?;
        if !sql.contains("INTO cache") {
            self.db_writes.set(self.db_writes.get() + n);
        }
        Ok(n as u64)
    }

    fn github(&self, r: &HttpRequest) -> Result<HttpResponse, String> {
        let resp = self.github_answer(r);
        let mut gh = self.gh.borrow_mut();
        if r.url.starts_with("https://uploads.test/") && std::mem::take(&mut gh.lose_upload_answer)
        {
            return Err("connection reset".into());
        }
        Ok(resp)
    }

    fn github_answer(&self, r: &HttpRequest) -> HttpResponse {
        let ok = |status: u16, v: Value| HttpResponse {
            status,
            body: serde_json::to_vec(&v).unwrap(),
        };
        let auth = r
            .headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str());
        assert!(
            r.headers.iter().any(|(k, _)| k == "user-agent"),
            "GitHub requires a User-Agent"
        );
        let mut gh = self.gh.borrow_mut();
        let api = "https://api.github.com/repos/octos-org/octos-crucible/";

        if r.url == "https://github.com/login/oauth/access_token" {
            let body = String::from_utf8(r.body.clone().unwrap()).unwrap();
            assert!(body.contains(&format!("client_secret={CLIENT_SECRET}")));
            return if body.contains("code=good") {
                ok(
                    200,
                    json!({"access_token": "gho_user", "token_type": "bearer"}),
                )
            } else {
                ok(200, json!({"error": "bad_verification_code"}))
            };
        }
        if r.url == "https://api.github.com/user" {
            assert_eq!(auth, Some("Bearer gho_user"));
            return ok(200, json!({"id": 42, "login": "octocat"}));
        }
        if let Some(rest) = r
            .url
            .strip_prefix("https://github.com/octos-org/octos-crucible/releases/download/")
        {
            // Public download: no token.
            assert_eq!(auth, None);
            let (tag, name) = rest.split_once('/').unwrap();
            let id = gh.releases.get(tag);
            return match id.and_then(|id| gh.assets.get(id)?.get(name)) {
                Some(bytes) => HttpResponse {
                    status: 200,
                    body: bytes.clone(),
                },
                None => ok(404, json!({"message": "Not Found"})),
            };
        }
        // Everything else is repo-scoped and uses the platform token.
        assert_eq!(
            auth,
            Some(format!("Bearer {GH_TOKEN}").as_str()),
            "{}",
            r.url
        );

        if let Some(name) = r
            .url
            .strip_prefix("https://uploads.test/releases/")
            .and_then(|rest| rest.split_once("/assets?name="))
        {
            let id: u64 = name.0.parse().unwrap();
            let list = gh.assets.entry(id).or_default();
            if list.contains_key(name.1) {
                return ok(422, json!({"errors": [{"code": "already_exists"}]}));
            }
            list.insert(name.1.to_string(), r.body.clone().unwrap());
            return ok(201, json!({"name": name.1}));
        }
        let path = r
            .url
            .strip_prefix(api)
            .unwrap_or_else(|| panic!("unexpected URL {}", r.url));
        match (r.method, path) {
            ("GET", p) if p.starts_with("releases/tags/") => {
                let tag = &p["releases/tags/".len()..];
                match gh.releases.get(tag) {
                    Some(id) => ok(200, release_json(*id)),
                    None => ok(404, json!({"message": "Not Found"})),
                }
            }
            ("POST", "releases") => {
                let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
                assert_eq!(body["prerelease"], true);
                let id = 100 + gh.releases.len() as u64;
                gh.releases
                    .insert(body["tag_name"].as_str().unwrap().into(), id);
                ok(201, release_json(id))
            }
            (
                "POST",
                p @ ("actions/workflows/eval.yml/dispatches"
                | "actions/workflows/score.yml/dispatches"),
            ) => {
                let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
                assert_eq!(body["ref"], "main");
                let eval_id = body["inputs"]["eval_id"].as_str().unwrap().to_string();
                let mut inputs = body["inputs"].clone();
                inputs["_workflow"] = json!(p.split('/').nth(2).unwrap());
                gh.dispatches.push(inputs);
                let id = 9000 + gh.runs.len() as u64;
                gh.runs.push((id, eval_id, "queued".into(), None));
                HttpResponse {
                    status: 204,
                    body: vec![],
                }
            }
            (
                "POST",
                p @ ("actions/workflows/taskset-pack.yml/dispatches"
                | "actions/workflows/plugin-pack.yml/dispatches"),
            ) => {
                let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
                let mut inputs = body["inputs"].clone();
                inputs["_workflow"] = json!(p.split('/').nth(2).unwrap());
                gh.dispatches.push(inputs);
                HttpResponse {
                    status: 204,
                    body: vec![],
                }
            }
            ("GET", p)
                if p.starts_with("actions/workflows/eval.yml/runs?")
                    || p.starts_with("actions/workflows/score.yml/runs?") =>
            {
                assert!(p.contains("event=workflow_dispatch"));
                let runs: Vec<Value> = gh.runs.iter().map(run_json).collect();
                ok(
                    200,
                    json!({"total_count": runs.len(), "workflow_runs": runs}),
                )
            }
            ("GET", p) if p.starts_with("actions/runs/") && p.ends_with("/jobs?per_page=100") => {
                ok(
                    200,
                    json!({"jobs": [
                        {"name": "build", "status": "completed"},
                        {"name": "generate (replica 1)", "status": "in_progress"}
                    ]}),
                )
            }
            ("GET", p) if p.starts_with("actions/runs/") => {
                let id: u64 = p["actions/runs/".len()..].parse().unwrap();
                let run = gh.runs.iter().find(|r| r.0 == id).unwrap();
                ok(200, run_json(run))
            }
            ("GET", "contents/tasksets?ref=main") => ok(
                200,
                json!([
                    {"type": "dir", "name": "github-full"},
                    {"type": "file", "name": "README.md"}
                ]),
            ),
            ("GET", "contents/tasksets/github-full/taskset.json?ref=main") => {
                let ts = json!({
                    "schema": 1, "name": "github-full", "version": "1.0",
                    "scorer": {"name": "playwright"},
                    "stages": [
                        {"id": "stage-1", "inputs": ["stage-1/requirements.md"], "output": "web_app", "time_limit_s": 3600, "expected_total": 30},
                        {"id": "stage-2", "inputs": ["stage-2/requirements.md"], "output": "web_app", "time_limit_s": 3600, "expected_total": 29}
                    ]
                });
                ok(
                    200,
                    json!({"sha": "abc123", "content": b64_encode(ts.to_string().as_bytes())}),
                )
            }
            (m, p) => panic!("unexpected GitHub call {m} {p}"),
        }
    }
}

fn release_json(id: u64) -> Value {
    json!({"id": id, "upload_url": format!("https://uploads.test/releases/{id}/assets{{?name,label}}")})
}

fn run_json(r: &(u64, String, String, Option<String>)) -> Value {
    json!({
        "id": r.0,
        "html_url": format!("https://github.com/octos-org/octos-crucible/actions/runs/{}", r.0),
        "status": r.2, "conclusion": r.3, "display_title": format!("eval {}", r.1)
    })
}

fn to_sqlite(a: &SqlArg) -> rusqlite::types::Value {
    use rusqlite::types::Value as V;
    match a {
        SqlArg::Null => V::Null,
        SqlArg::Int(i) => V::Integer(*i),
        SqlArg::Real(f) => V::Real(*f),
        SqlArg::Text(s) => V::Text(s.clone()),
    }
}

impl Backend for Mock {
    async fn db_query(&self, sql: &str, args: &[SqlArg]) -> Result<Vec<Row>, String> {
        use rusqlite::types::ValueRef;
        let mut stmt = self.db.prepare(sql).map_err(|e| e.to_string())?;
        let names: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(args.iter().map(to_sqlite)))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let mut row = Row::new();
            for (i, name) in names.iter().enumerate() {
                let v = match r.get_ref(i).map_err(|e| e.to_string())? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(i) => json!(i),
                    ValueRef::Real(f) => json!(f),
                    ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    ValueRef::Blob(_) => panic!("no blobs in the schema"),
                };
                row.insert(name.clone(), v);
            }
            out.push(row);
        }
        Ok(out)
    }
    async fn db_exec(&self, sql: &str, args: &[SqlArg]) -> Result<u64, String> {
        self.write_failure()?;
        self.exec(sql, args)
    }
    async fn db_batch(&self, stmts: &[Stmt]) -> Result<(), String> {
        self.write_failure()?;
        self.db.execute_batch("BEGIN").unwrap();
        for s in stmts {
            if let Err(e) = self.exec(s.sql, &s.args) {
                self.db.execute_batch("ROLLBACK").unwrap();
                return Err(e);
            }
        }
        self.db.execute_batch("COMMIT").unwrap();
        Ok(())
    }
    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, String> {
        self.github(&req)
    }
    fn now_s(&self) -> u64 {
        self.now.get()
    }
    fn random_bytes(&self, n: usize) -> Vec<u8> {
        // Deterministic, but distinct per call.
        let r = self.rng.get();
        self.rng.set(r.wrapping_add(1));
        vec![r; n]
    }
    fn log(&self, line: &str) {
        self.logs.borrow_mut().push(line.to_owned());
    }
}

fn config(dev: bool) -> Config {
    Config::from_lookup(|name| {
        Some(
            match name {
                "PAGES_ORIGIN" => PAGES,
                "PAGES_URL" => "https://octos-org.github.io/octos-crucible/",
                "GITHUB_REPO" => "octos-org/octos-crucible",
                "ADMIN_GITHUB_IDS" => "1",
                "GITHUB_CLIENT_ID" => "Iv1.client",
                "GITHUB_CLIENT_SECRET" => CLIENT_SECRET,
                "GITHUB_TOKEN" => GH_TOKEN,
                "SESSION_HMAC_KEY" => SESSION_KEY,
                "CRUCIBLE_WORKER_TOKEN" => WORKER_TOKEN,
                "DEV_AUTH" if dev => "1",
                _ => return None,
            }
            .to_string(),
        )
    })
    .unwrap()
}

struct T {
    mock: Mock,
    cfg: Config,
}

impl T {
    fn new() -> T {
        T {
            mock: Mock::new(),
            cfg: config(false),
        }
    }

    fn call(&self, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let req = Req {
            method: method.into(),
            origin: "https://crucible.example.workers.dev".into(),
            host: "crucible.example.workers.dev".into(),
            path: path.into(),
            query: query.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
                .collect(),
            body: body.to_vec(),
        };
        block_on(handle(&self.mock, &self.cfg, &req))
    }

    fn as_user(&self, method: &str, target: &str, token: &str, body: &[u8]) -> Resp {
        let auth = format!("Bearer {token}");
        self.call(method, target, &[("Authorization", &auth)], body)
    }
}

fn json_of(r: &Resp) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

fn err_code(r: &Resp) -> String {
    json_of(r)["error"]["code"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn sealed(payload: &[u8]) -> Vec<u8> {
    let mut b = Envelope::new(keys::current().key_id.clone()).header_line();
    b.extend_from_slice(b"age-encryption.org/v1\n");
    b.extend_from_slice(payload);
    // Real ciphertext is binary (not UTF-8).
    b.extend_from_slice(&[0xff, 0xfe, 0x00, 0x80]);
    b
}

fn token(gid: u64, login: &str) -> String {
    issue_session(SESSION_KEY.as_bytes(), gid, login, NOW)
}

#[test]
fn oauth_login() {
    let t = T::new();
    let r = t.call("GET", "/auth/login", &[], b"");
    assert_eq!(r.status, 302);
    let loc = r.header("location").unwrap();
    assert!(loc.starts_with("https://github.com/login/oauth/authorize?client_id=Iv1.client"));
    assert!(
        loc.contains("redirect_uri=https%3A%2F%2Fcrucible.example.workers.dev%2Fauth%2Fcallback")
    );
    let cookie = r.header("set-cookie").unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"));
    let nonce = cookie.split(';').next().unwrap().to_string();
    let state = loc
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();

    // Good round trip.
    let r = t.call(
        "GET",
        &format!("/auth/callback?code=good&state={state}"),
        &[("Cookie", &nonce)],
        b"",
    );
    assert_eq!(r.status, 302);
    let loc = r.header("location").unwrap();
    let tok = loc
        .strip_prefix("https://octos-org.github.io/octos-crucible/#token=")
        .expect(loc);
    let me = t.as_user("GET", "/me", tok, b"");
    assert_eq!(
        json_of(&me),
        json!({"github_id": 42, "login": "octocat", "is_admin": false})
    );

    // Missing/foreign cookie, bad code, denied, banned.
    let r = t.call(
        "GET",
        &format!("/auth/callback?code=good&state={state}"),
        &[],
        b"",
    );
    assert!(
        r.header("location")
            .unwrap()
            .ends_with("#error=oauth_state")
    );
    let r = t.call(
        "GET",
        &format!("/auth/callback?code=good&state={state}"),
        &[("Cookie", "crucible_oauth=other")],
        b"",
    );
    assert!(
        r.header("location")
            .unwrap()
            .ends_with("#error=oauth_state")
    );
    let r = t.call(
        "GET",
        &format!("/auth/callback?code=bad&state={state}"),
        &[("Cookie", &nonce)],
        b"",
    );
    assert!(
        r.header("location")
            .unwrap()
            .ends_with("#error=oauth_failed")
    );
    let r = t.call("GET", "/auth/callback?error=access_denied", &[], b"");
    assert!(
        r.header("location")
            .unwrap()
            .ends_with("#error=oauth_denied")
    );
    // Expired state.
    t.mock.now.set(NOW + 601);
    let r = t.call(
        "GET",
        &format!("/auth/callback?code=good&state={state}"),
        &[("Cookie", &nonce)],
        b"",
    );
    assert!(
        r.header("location")
            .unwrap()
            .ends_with("#error=oauth_state")
    );
    t.mock.now.set(NOW);
    t.mock
        .sql("INSERT INTO bans (github_id, by_id, at) VALUES (42, 1, '2026-10-03T00:00:00Z')");
    let r = t.call(
        "GET",
        &format!("/auth/callback?code=good&state={state}"),
        &[("Cookie", &nonce)],
        b"",
    );
    assert!(r.header("location").unwrap().ends_with("#error=banned"));

    // The OAuth code and tokens never reach the log.
    for line in t.mock.logs.borrow().iter() {
        assert!(!line.contains("good") && !line.contains("gho_") && !line.contains(tok));
    }
}

#[test]
fn auth_and_cors() {
    let t = T::new();
    assert_eq!(t.call("GET", "/me", &[], b"").status, 401);
    assert_eq!(t.as_user("GET", "/me", "v1.junk.junk", b"").status, 401);
    let expired = issue_session(SESSION_KEY.as_bytes(), 42, "octocat", NOW - 2 * 86_400);
    assert_eq!(t.as_user("GET", "/me", &expired, b"").status, 401);
    let admin = t.as_user("GET", "/me", &token(1, "admin"), b"");
    assert_eq!(json_of(&admin)["is_admin"], true);

    let pre = t.call(
        "OPTIONS",
        "/evals",
        &[("Origin", PAGES), ("Access-Control-Request-Method", "POST")],
        b"",
    );
    assert_eq!(pre.status, 204);
    assert_eq!(pre.header("access-control-allow-origin"), Some(PAGES));
    assert!(
        pre.header("access-control-allow-headers")
            .unwrap()
            .contains("X-Upload-Kind")
    );
    let evil = t.call(
        "OPTIONS",
        "/evals",
        &[("Origin", "https://evil.example")],
        b"",
    );
    assert_eq!(evil.header("access-control-allow-origin"), None);
    let get = t.call("GET", "/pubkey", &[("Origin", PAGES)], b"");
    assert_eq!(get.header("access-control-allow-origin"), Some(PAGES));
    assert_eq!(json_of(&get)["key_id"], keys::current().key_id);
    // Internal endpoints never answer cross-origin.
    let internal = t.call("OPTIONS", "/internal/cred/x", &[("Origin", PAGES)], b"");
    assert_eq!(internal.header("access-control-allow-origin"), None);

    assert_eq!(err_code(&t.call("GET", "/nope", &[], b"")), "not_found");
    assert_eq!(t.call("PUT", "/evals", &[], b"").status, 405);
}

#[test]
fn dev_login_only_in_dev_on_localhost() {
    let mut t = T::new();
    assert_eq!(
        t.call("GET", "/auth/dev-login?github_id=5&login=dev", &[], b"")
            .status,
        404
    );
    t.cfg = config(true);
    // Still refused for a non-local host.
    assert_eq!(
        t.call("GET", "/auth/dev-login?github_id=5&login=dev", &[], b"")
            .status,
        404
    );
    let req = Req {
        method: "GET".into(),
        origin: "http://localhost:8787".into(),
        host: "localhost".into(),
        path: "/auth/dev-login".into(),
        query: "github_id=5&login=dev".into(),
        ..Default::default()
    };
    let r = block_on(handle(&t.mock, &t.cfg, &req));
    assert_eq!(r.status, 200);
    let tok = json_of(&r)["token"].as_str().unwrap().to_string();
    assert_eq!(json_of(&t.as_user("GET", "/me", &tok, b""))["github_id"], 5);
}

#[test]
fn full_flow() {
    let t = T::new();
    let alice = token(42, "octocat");
    let bob = token(43, "bob");
    let admin = token(1, "admin");

    // Tasksets (public).
    let ts = t.call("GET", "/tasksets", &[], b"");
    assert_eq!(
        json_of(&ts),
        json!([{"name": "github-full", "version": "1.0", "stages": [
            {"name": "stage-1", "time_limit_s": 3600, "total": 30},
            {"name": "stage-2", "time_limit_s": 3600, "total": 29}
        ]}])
    );

    // Upload.
    let up = |tok: &str, kind: &str, body: &[u8]| {
        let auth = format!("Bearer {tok}");
        t.call(
            "POST",
            "/uploads",
            &[("Authorization", &auth), ("X-Upload-Kind", kind)],
            body,
        )
    };
    assert_eq!(
        err_code(&up(&alice, "agent", b"PK\x03\x04zip")),
        "not_sealed"
    );
    assert_eq!(err_code(&up(&alice, "zip", &sealed(b"x"))), "bad_request");
    let mut wrong = Envelope::new("ffffffffffffffff").header_line();
    wrong.extend_from_slice(b"x");
    assert_eq!(err_code(&up(&alice, "agent", &wrong)), "wrong_key");
    let pkg = sealed(b"agent package ciphertext");
    let r = up(&alice, "agent", &pkg);
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let hash = json_of(&r)["hash"].as_str().unwrap().to_string();
    assert_eq!(hash, sha256_hex(&pkg));
    let tag = release_tag(&hash).unwrap();
    assert!(t.mock.gh.borrow().releases.contains_key(&tag));
    assert_eq!(up(&alice, "agent", &pkg).status, 200); // idempotent
    assert_eq!(up(&bob, "agent", &pkg).status, 409); // not claimable
    // Bytes already in the store but not uploaded by this user.
    let foreign = sealed(b"someone's output");
    {
        let mut gh = t.mock.gh.borrow_mut();
        let id = 777;
        gh.releases
            .insert(release_tag(&sha256_hex(&foreign)).unwrap(), id);
        gh.assets
            .entry(id)
            .or_default()
            .insert(sha256_hex(&foreign), foreign.clone());
    }
    t.mock.sql(&format!(
        "DELETE FROM cache WHERE key = 'release/{}'",
        release_tag(&sha256_hex(&foreign)).unwrap()
    ));
    assert_eq!(up(&bob, "agent", &foreign).status, 409);
    assert_eq!(
        t.call("POST", "/uploads", &[("X-Upload-Kind", "agent")], &pkg)
            .status,
        401
    );

    // Submit.
    let cred = sealed(b"model key ciphertext");
    let eval = json!({
        "mode": "agent", "eval_id": EID, "upload_hash": hash, "taskset": "github-full",
        "model": "glm-5.3", "replicas": 2, "budget": {"max_cost_usd": 5.0},
        "cred_envelope": b64_encode(&cred), "score_public": true, "consent": true
    });
    let body = serde_json::to_vec(&eval).unwrap();
    // Bob cannot use Alice's upload.
    let mut bobs = eval.clone();
    bobs["eval_id"] = json!("1b7e6a52-1f3c-4d2a-9e8b-7c6d5e4f3a21");
    assert_eq!(
        t.as_user("POST", "/evals", &bob, &serde_json::to_vec(&bobs).unwrap())
            .status,
        403
    );
    let mut no_consent = eval.clone();
    no_consent["consent"] = json!(false);
    assert_eq!(
        err_code(&t.as_user(
            "POST",
            "/evals",
            &alice,
            &serde_json::to_vec(&no_consent).unwrap()
        )),
        "consent_required"
    );
    let mut too_many = eval.clone();
    too_many["stages"] = json!(3);
    assert_eq!(
        t.as_user(
            "POST",
            "/evals",
            &alice,
            &serde_json::to_vec(&too_many).unwrap()
        )
        .status,
        400
    );
    let mut wrong_kind = eval.clone();
    wrong_kind["mode"] = json!("app");
    wrong_kind
        .as_object_mut()
        .unwrap()
        .retain(|k, _| !["model", "replicas", "budget", "cred_envelope"].contains(&k.as_str()));
    assert_eq!(
        t.as_user(
            "POST",
            "/evals",
            &alice,
            &serde_json::to_vec(&wrong_kind).unwrap()
        )
        .status,
        400
    );
    let r = t.as_user("POST", "/evals", &alice, &body);
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json_of(&r), json!({"eval_id": EID}));
    assert_eq!(t.as_user("POST", "/evals", &alice, &body).status, 409);

    let inputs = t.mock.gh.borrow().dispatches[0].clone();
    let results_url = format!("https://crucible.example.workers.dev/internal/results/{EID}");
    assert_eq!(
        inputs,
        json!({
            "_workflow": "eval.yml",
            "agent_source": format!("blob:{hash}"), "taskset": "github-full",
            "model": "glm-5.3", "endpoint": "", "replicas": "2",
            "cred_source": "workers-kv", "eval_id": EID, "score_public": "true",
            "owner": "42:octocat",
            "options": json!({"stages": 2, "results_url": results_url, "budget": {"max_cost_usd": 5.0}}).to_string()
        })
    );
    let input_text = inputs.to_string();
    assert!(!input_text.contains(&b64_encode(&cred)) && !input_text.contains("ciphertext"));

    // Status before and while running (estimated from GitHub).
    let detail = |tok: &str| json_of(&t.as_user("GET", &format!("/evals/{EID}"), tok, b""));
    let r = detail(&alice);
    assert_eq!(r["status"], "queued");
    assert!(
        r["run_url"]
            .as_str()
            .unwrap()
            .ends_with("/actions/runs/9000")
    );
    t.mock.gh.borrow_mut().runs[0].2 = "in_progress".into();
    assert_eq!(detail(&alice)["status"], "running:stage-1");
    // Precise progress reported by the workflow wins and never regresses.
    let status_path = format!("/internal/status/{EID}");
    let report = |s: &str| {
        t.as_user(
            "POST",
            &status_path,
            WORKER_TOKEN,
            &serde_json::to_vec(&json!({"status": s})).unwrap(),
        )
    };
    assert_eq!(report("running:stage-2").status, 200);
    assert_eq!(detail(&alice)["status"], "running:stage-2");
    assert_eq!(report("done").status, 400);
    assert_eq!(report("running:Stage 2").status, 400);
    assert_eq!(
        t.as_user("POST", &status_path, &alice, b"{\"status\":\"scoring\"}")
            .status,
        401
    );
    assert_eq!(
        t.as_user("GET", &format!("/evals/{EID}"), &bob, b"").status,
        403
    );
    assert_eq!(
        t.as_user("GET", &format!("/evals/{EID}"), &admin, b"")
            .status,
        200
    );
    assert_eq!(
        t.as_user("GET", "/evals/not-a-uuid", &alice, b"").status,
        400
    );

    // Actions: fetch the credential, then delete it.
    let cred_path = format!("/internal/cred/{EID}");
    assert_eq!(t.call("GET", &cred_path, &[], b"").status, 401);
    assert_eq!(t.as_user("GET", &cred_path, &alice, b"").status, 401);
    let r = t.as_user("GET", &cred_path, WORKER_TOKEN, b"");
    assert_eq!(r.status, 200);
    assert_eq!(r.body, cred);
    assert_eq!(
        t.as_user("DELETE", &cred_path, WORKER_TOKEN, b"").status,
        204
    );
    assert_eq!(t.as_user("GET", &cred_path, WORKER_TOKEN, b"").status, 404);

    // Not downloadable yet.
    assert_eq!(
        err_code(&t.as_user("GET", &format!("/evals/{EID}/download"), &alice, b"")),
        "not_ready"
    );

    // Results.
    let zip = "c0".repeat(32);
    let manifest = json!({
        "schema": 1, "eval_id": EID, "created_at": "2026-10-03T00:00:00Z",
        "taskset": "github-full", "agent": {"name": "uploaded", "version": "1"},
        "model": "glm-5.3", "public": true,
        "replicas": [{"replica": 1, "stages": [
            {"stage": "stage-1", "score": {"status": "failed", "passed": 27, "total": 30},
             "usage": {"requests": 3, "prompt_tokens": 10, "cached_tokens": 2, "completion_tokens": 5, "reasoning_tokens": 0}}
        ]}],
        "download": {"sha256": zip}
    });
    let results = format!("/internal/results/{EID}");
    let mbody = serde_json::to_vec(&manifest).unwrap();
    assert_eq!(t.as_user("POST", &results, &alice, &mbody).status, 401);
    assert_eq!(t.as_user("POST", &results, WORKER_TOKEN, b"{}").status, 400);
    // A partial manifest while the run continues.
    let mut partial = manifest.clone();
    partial["status"] = json!("scoring");
    partial.as_object_mut().unwrap().remove("download");
    let r = t.as_user(
        "POST",
        &results,
        WORKER_TOKEN,
        &serde_json::to_vec(&partial).unwrap(),
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let r = json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(r["status"], "scoring");
    assert_eq!(r["total_score"], 0.9);
    assert_eq!(r["download_available"], false);

    let r = t.as_user("POST", &results, WORKER_TOKEN, &mbody);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));

    let r = json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(r["status"], "done");
    assert_eq!(
        r["manifest"]["replicas"][0]["stages"][0]["score"]["passed"],
        27
    );
    assert_eq!(r["total_score"], 0.9);
    assert_eq!(r["download_available"], true);
    assert!(r["manifest"].get("download").is_none());
    // Settled: further progress reports are refused.
    assert_eq!(report("scoring").status, 409);

    let list = json_of(&t.as_user("GET", "/evals", &alice, b""));
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["eval_id"], EID);
    assert_eq!(list[0]["status"], "done");
    assert_eq!(list[0]["total_score"], 0.9);
    assert_eq!(json_of(&t.as_user("GET", "/evals", &bob, b"")), json!([]));
    assert_eq!(t.as_user("GET", "/evals?all=1", &bob, b"").status, 403);
    assert_eq!(
        json_of(&t.as_user("GET", "/evals?all=1", &admin, b""))
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Download.
    let dl = format!("/evals/{EID}/download");
    let r = t.as_user("GET", &dl, &alice, b"");
    assert_eq!(r.status, 302);
    assert_eq!(
        r.header("location").unwrap(),
        format!(
            "https://github.com/octos-org/octos-crucible/releases/download/{}/{zip}",
            release_tag(&zip).unwrap()
        )
    );
    let r = t.call(
        "GET",
        &dl,
        &[
            ("Authorization", &format!("Bearer {alice}")),
            ("Accept", "application/json"),
        ],
        b"",
    );
    assert!(json_of(&r)["url"].as_str().unwrap().ends_with(&zip));
    assert_eq!(t.as_user("GET", &dl, &bob, b"").status, 403);

    // Nothing secret in the log.
    for line in t.mock.logs.borrow().iter() {
        for secret in [alice.as_str(), WORKER_TOKEN, GH_TOKEN, &b64_encode(&cred)] {
            assert!(!line.contains(secret), "{line}");
        }
    }
}

#[test]
fn app_mode_and_failures() {
    let t = T::new();
    let alice = token(42, "octocat");
    let art = sealed(b"site zip");
    let r = t.call(
        "POST",
        "/uploads",
        &[
            ("Authorization", &format!("Bearer {alice}")),
            ("X-Upload-Kind", "app"),
        ],
        &art,
    );
    let hash = json_of(&r)["hash"].as_str().unwrap().to_string();
    let eval = json!({
        "mode": "app", "eval_id": EID, "upload_hash": hash, "taskset": "github-full",
        "stages": 2, "score_public": false, "consent": true
    });
    let r = t.as_user(
        "POST",
        "/evals",
        &alice,
        &serde_json::to_vec(&eval).unwrap(),
    );
    assert_eq!(r.status, 201);
    let inputs = t.mock.gh.borrow().dispatches[0].clone();
    assert_eq!(
        inputs,
        json!({
            "_workflow": "score.yml", "eval_id": EID,
            "artifact_source": format!("blob:{hash}"), "taskset": "github-full",
            "stage": "2", "cred_source": "none", "model": "", "score_public": "false",
            "owner": "42:octocat",
            "results_url": format!("https://crucible.example.workers.dev/internal/results/{EID}")
        })
    );
    assert_eq!(t.mock.count("SELECT COUNT(*) FROM creds"), 0);
    // While the scoring job runs.
    t.mock.gh.borrow_mut().runs[0].2 = "in_progress".into();
    let r = json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(r["status"], "running:stage-2");
    assert_eq!(r["stage_names"], json!(["stage-2"]));

    // A failed run settles the eval.
    t.mock.gh.borrow_mut().runs[0].2 = "completed".into();
    t.mock.gh.borrow_mut().runs[0].3 = Some("failure".into());
    let r = json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(r["status"], "failed");
    let list = json_of(&t.as_user("GET", "/evals", &alice, b""));
    assert_eq!(list[0]["status"], "failed");

    // Stage numbers are checked against the taskset.
    let mut bad = eval.clone();
    bad["eval_id"] = json!("2b7e6a52-1f3c-4d2a-9e8b-7c6d5e4f3a21");
    bad["stages"] = json!(3);
    assert_eq!(
        t.as_user("POST", "/evals", &alice, &serde_json::to_vec(&bad).unwrap())
            .status,
        400
    );
}

#[test]
fn success_without_results_fails_after_grace() {
    let t = T::new();
    let alice = token(42, "octocat");
    let art = sealed(b"site");
    let r = t.call(
        "POST",
        "/uploads",
        &[
            ("Authorization", &format!("Bearer {alice}")),
            ("X-Upload-Kind", "app"),
        ],
        &art,
    );
    let hash = json_of(&r)["hash"].as_str().unwrap().to_string();
    let eval = json!({
        "mode": "app", "eval_id": EID, "upload_hash": hash, "taskset": "github-full",
        "stages": 1, "score_public": false, "consent": true
    });
    assert_eq!(
        t.as_user(
            "POST",
            "/evals",
            &alice,
            &serde_json::to_vec(&eval).unwrap()
        )
        .status,
        201
    );
    t.mock.gh.borrow_mut().runs[0].2 = "completed".into();
    t.mock.gh.borrow_mut().runs[0].3 = Some("success".into());
    let status =
        || json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""))["status"].clone();
    assert_eq!(status(), "queued");
    t.mock.now.set(NOW + 300);
    assert_eq!(status(), "queued");
    t.mock.now.set(NOW + 601);
    assert_eq!(status(), "failed");
}

fn app_manifest(passed: u32) -> Value {
    json!({
        "schema": 1, "eval_id": EID, "created_at": "2026-10-03T00:00:00Z",
        "taskset": "github-full", "agent": {"name": "uploaded", "version": "1"},
        "model": "app", "public": false,
        "replicas": [{"replica": 1, "stages": [
            {"stage": "stage-1", "score": {"status": "passed", "passed": passed, "total": 1},
             "usage": {"requests": 0, "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0, "reasoning_tokens": 0}}
        ]}]
    })
}

/// Uploads a site and submits an app-mode eval of stage 1 as `EID`.
fn submit_app(t: &T, tok: &str) {
    let hash = json_of(&upload_as(t, tok, &sealed(b"site")))["hash"]
        .as_str()
        .unwrap()
        .to_string();
    let eval = json!({
        "mode": "app", "eval_id": EID, "upload_hash": hash, "taskset": "github-full",
        "stages": 1, "score_public": false, "consent": true
    });
    let r = t.as_user("POST", "/evals", tok, &serde_json::to_vec(&eval).unwrap());
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
}

/// Settled is final: progress reports, GitHub estimates and late partial
/// results never move a done eval, and the row updates refuse to as well.
#[test]
fn settled_evals_stay_settled() {
    let t = T::new();
    let alice = token(42, "octocat");
    submit_app(&t, &alice);
    let detail = || json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    let post = |m: &Value| {
        t.as_user(
            "POST",
            &format!("/internal/results/{EID}"),
            WORKER_TOKEN,
            &serde_json::to_vec(m).unwrap(),
        )
    };
    let report = |s: &str| {
        t.as_user(
            "POST",
            &format!("/internal/status/{EID}"),
            WORKER_TOKEN,
            &serde_json::to_vec(&json!({"status": s})).unwrap(),
        )
    };
    t.mock.gh.borrow_mut().runs[0].2 = "in_progress".into();
    assert_eq!(detail()["status"], "running:stage-1");

    let r = post(&app_manifest(1));
    assert_eq!(json_of(&r)["status"], "done");
    // A late partial delivery is ignored.
    let mut partial = app_manifest(0);
    partial["status"] = json!("scoring");
    assert_eq!(json_of(&post(&partial))["status"], "done");
    assert_eq!(
        t.mock
            .count("SELECT COUNT(*) FROM results WHERE status = 'done' AND total_score = 1.0"),
        1
    );
    assert_eq!(report("scoring").status, 409);
    // GitHub calling the run failed does not undo the results.
    t.mock.gh.borrow_mut().runs[0].2 = "completed".into();
    t.mock.gh.borrow_mut().runs[0].3 = Some("failure".into());
    let r = detail();
    assert_eq!(
        (r["status"].as_str(), r["total_score"].as_f64()),
        (Some("done"), Some(1.0))
    );
    let list = json_of(&t.as_user("GET", "/evals", &alice, b""));
    assert_eq!(list[0]["status"], "done");
    assert_eq!(list[0]["total_score"], 1.0);

    // Row level: a settled record is never updated.
    t.mock.sql(&format!(
        "UPDATE evals SET status = 'failed' WHERE eval_id = '{EID}'"
    ));
    let db = Db(&t.mock);
    assert!(!block_on(db.set_status(EID, "scoring", "2026-10-03T00:00:00Z", None)).unwrap());
    assert_eq!(
        t.mock.count(&format!(
            "SELECT COUNT(*) FROM evals WHERE eval_id = '{EID}' AND status = 'failed'"
        )),
        1
    );
}

/// A run reported failed, whose final results still arrive: the results
/// win (as before), and the list follows without any further write.
#[test]
fn results_after_reported_failure() {
    let t = T::new();
    let alice = token(42, "octocat");
    submit_app(&t, &alice);
    let r = t.as_user(
        "POST",
        &format!("/internal/status/{EID}"),
        WORKER_TOKEN,
        br#"{"status":"failed"}"#,
    );
    assert_eq!(r.status, 200);
    let detail = || json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(detail()["status"], "failed");
    // Results are still accepted (a rerun or a late delivery).
    let r = t.as_user(
        "POST",
        &format!("/internal/results/{EID}"),
        WORKER_TOKEN,
        &serde_json::to_vec(&app_manifest(1)).unwrap(),
    );
    assert_eq!(json_of(&r)["status"], "done");
    assert_eq!(detail()["status"], "done");
    let list = json_of(&t.as_user("GET", "/evals", &alice, b""));
    assert_eq!(list[0]["status"], "done");
}

#[test]
fn cred_expires_after_a_day() {
    let t = T::new();
    let alice = token(42, "octocat");
    let pkg = sealed(b"pkg");
    let r = t.call(
        "POST",
        "/uploads",
        &[
            ("Authorization", &format!("Bearer {alice}")),
            ("X-Upload-Kind", "agent"),
        ],
        &pkg,
    );
    let hash = json_of(&r)["hash"].as_str().unwrap().to_string();
    let eval = json!({
        "mode": "agent", "eval_id": EID, "upload_hash": hash, "taskset": "github-full",
        "model": "m", "cred_envelope": b64_encode(&sealed(b"k")), "score_public": false, "consent": true
    });
    assert_eq!(
        t.as_user(
            "POST",
            "/evals",
            &alice,
            &serde_json::to_vec(&eval).unwrap()
        )
        .status,
        201
    );
    let path = format!("/internal/cred/{EID}");
    assert_eq!(t.as_user("GET", &path, WORKER_TOKEN, b"").status, 200);
    // Expired: absent on read, deleted by the cron.
    t.mock.now.set(NOW + 86_400);
    assert_eq!(t.as_user("GET", &path, WORKER_TOKEN, b"").status, 404);
    assert_eq!(t.mock.count("SELECT COUNT(*) FROM creds"), 1);
    block_on(crucible_worker::app::scheduled(&t.mock, None));
    assert_eq!(t.mock.count("SELECT COUNT(*) FROM creds"), 0);
}

#[test]
fn bans() {
    let t = T::new();
    let alice = token(42, "octocat");
    let admin = token(1, "admin");
    let ban = |tok: &str, id: u64| {
        t.as_user(
            "POST",
            "/admin/ban",
            tok,
            &serde_json::to_vec(&json!({"github_id": id, "reason": "abuse"})).unwrap(),
        )
    };
    assert_eq!(ban(&alice, 43).status, 403);
    assert_eq!(ban(&admin, 1).status, 400); // admins cannot be banned
    assert_eq!(ban(&admin, 42).status, 200);
    let r = t.as_user("GET", "/me", &alice, b"");
    assert_eq!((r.status, err_code(&r).as_str()), (403, "banned"));
    assert_eq!(
        t.call(
            "POST",
            "/uploads",
            &[
                ("Authorization", &format!("Bearer {alice}")),
                ("X-Upload-Kind", "agent")
            ],
            &sealed(b"x"),
        )
        .status,
        403
    );
    let r = t.as_user(
        "POST",
        "/admin/unban",
        &admin,
        &serde_json::to_vec(&json!({"github_id": 42})).unwrap(),
    );
    assert_eq!(r.status, 200);
    assert_eq!(t.as_user("GET", "/me", &alice, b"").status, 200);
    assert_eq!(
        t.as_user("POST", "/admin/ban", &admin, b"{\"github_id\":\"x\"}")
            .status,
        400
    );
    // util is part of the public surface the tests rely on.
    assert_eq!(util::rfc3339(NOW), "2026-10-03T00:00:00Z");
}

#[test]
fn api_tokens() {
    let t = T::new();
    let alice = token(42, "octocat");
    let admin = token(1, "admin");

    // Created with a session; the plaintext is returned once.
    let r = t.as_user("POST", "/tokens", &alice, br#"{"name":"laptop"}"#);
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let created = json_of(&r);
    let cli = created["token"].as_str().unwrap().to_string();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(cli.starts_with("crt_") && cli.contains(&id));
    assert_eq!(created["name"], "laptop");
    // D1 holds only the hash.
    assert_eq!(
        t.mock.count(&format!(
            "SELECT COUNT(*) FROM tokens WHERE id || name || hash || login LIKE '%{}%'",
            &cli[20..]
        )),
        0
    );
    assert_eq!(
        t.mock.count(&format!(
            "SELECT COUNT(*) FROM tokens WHERE hash = '{}'",
            sha256_hex(cli.as_bytes())
        )),
        1
    );
    let r = t.as_user("POST", "/tokens", &alice, b"");
    assert_eq!((r.status, json_of(&r)["name"].as_str()), (201, Some("cli")));
    let second = json_of(&r)["id"].as_str().unwrap().to_string();
    assert_ne!(second, id);
    let list = json_of(&t.as_user("GET", "/tokens", &alice, b""));
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(list[0].get("token").is_none() && list[0].get("hash").is_none());

    // The token works on user endpoints as its owner...
    let me = json_of(&t.as_user("GET", "/me", &cli, b""));
    assert_eq!(
        (me["github_id"].as_u64(), me["is_admin"].as_bool()),
        (Some(42), Some(false))
    );
    let r = t.call(
        "POST",
        "/uploads",
        &[
            ("Authorization", &format!("Bearer {cli}")),
            ("X-Upload-Kind", "app"),
        ],
        &sealed(b"app via token"),
    );
    assert_eq!(r.status, 201);
    assert_eq!(t.as_user("GET", "/evals", &cli, b"").status, 200);
    // ...but cannot manage tokens, and is never an admin.
    assert_eq!(t.as_user("POST", "/tokens", &cli, b"").status, 403);
    assert_eq!(t.as_user("GET", "/tokens", &cli, b"").status, 403);
    let r = t.as_user("POST", "/tokens", &admin, b"");
    let admin_cli = json_of(&r)["token"].as_str().unwrap().to_string();
    let ban = serde_json::to_vec(&json!({"github_id": 42})).unwrap();
    assert_eq!(
        t.as_user("POST", "/admin/ban", &admin_cli, &ban).status,
        403
    );
    assert_eq!(
        t.as_user("GET", "/evals?all=1", &admin_cli, b"").status,
        403
    );
    // Internal endpoints ignore user tokens entirely.
    assert_eq!(
        t.as_user("GET", &format!("/internal/cred/{EID}"), &cli, b"")
            .status,
        401
    );

    // Tampered or unknown tokens are rejected.
    let mut forged = cli.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == '0' { '1' } else { '0' });
    assert_eq!(t.as_user("GET", "/me", &forged, b"").status, 401);

    // Only the owner can revoke; revoked tokens stop working.
    assert_eq!(
        t.as_user("DELETE", &format!("/tokens/{id}"), &admin, b"")
            .status,
        404
    );
    assert_eq!(
        t.as_user("DELETE", &format!("/tokens/{id}"), &alice, b"")
            .status,
        204
    );
    assert_eq!(t.as_user("GET", "/me", &cli, b"").status, 401);
    let list = json_of(&t.as_user("GET", "/tokens", &alice, b""));
    assert_eq!(list.as_array().unwrap().len(), 1);

    // A banned owner's token is refused too.
    let r = t.as_user("POST", "/tokens", &alice, b"");
    let cli2 = json_of(&r)["token"].as_str().unwrap().to_string();
    assert_eq!(t.as_user("POST", "/admin/ban", &admin, &ban).status, 200);
    assert_eq!(err_code(&t.as_user("GET", "/me", &cli2, b"")), "banned");
}

/// A packed taskset.json as the taskset-pack workflow posts it.
fn packed(id: &str) -> Value {
    let blob = json!({"sha256": "ab".repeat(32), "key_id": "1ffa702796eb5ee8"});
    json!({
        "schema": 1, "name": id, "title": "my-tasks", "scorer": {"name": "playwright"},
        "total_time_limit_s": 1200,
        "stages": [
            {"id": "s1", "inputs_blob": blob, "tests_blob": blob, "output": "web-app", "time_limit_s": 600, "expected_total": 3},
            {"id": "s2", "inputs_blob": blob, "tests_blob": blob, "output": "web-app", "time_limit_s": 600, "expected_total": 4}
        ]
    })
}

#[test]
fn user_tasksets() {
    let t = T::new();
    let alice = token(42, "octocat");
    let bob = token(43, "bob");
    let admin = token(1, "admin");
    let internal = format!("Bearer {WORKER_TOKEN}");
    let up = |tok: &str, kind: &str, body: &[u8]| {
        let r = t.call(
            "POST",
            "/uploads",
            &[
                ("Authorization", &format!("Bearer {tok}")),
                ("X-Upload-Kind", kind),
            ],
            body,
        );
        assert!(r.status < 300, "{}", String::from_utf8_lossy(&r.body));
        json_of(&r)["hash"].as_str().unwrap().to_string()
    };
    let register = |tok: &str, hash: &str| {
        let body = serde_json::to_vec(&json!({"upload_hash": hash})).unwrap();
        t.as_user("POST", "/tasksets", tok, &body)
    };
    let names = |tok: Option<&str>, q: &str| -> Vec<String> {
        let r = match tok {
            Some(tok) => t.as_user("GET", &format!("/tasksets{q}"), tok, b""),
            None => t.call("GET", &format!("/tasksets{q}"), &[], b""),
        };
        json_of(&r)
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap().to_string())
            .collect()
    };

    // Register: only one's own taskset uploads.
    let zip = up(&alice, "taskset", &sealed(b"taskset zip"));
    let agent = up(&alice, "agent", &sealed(b"agent zip"));
    assert_eq!(err_code(&register(&alice, &agent)), "bad_request");
    assert_eq!(register(&bob, &zip).status, 403);
    assert_eq!(t.call("POST", "/tasksets", &[], b"{}").status, 401);
    let r = register(&alice, &zip);
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let id = json_of(&r)["id"].as_str().unwrap().to_string();
    assert!(crucible_core::taskset::is_user_taskset_id(&id), "{id}");
    assert_eq!(json_of(&r)["status"], "packing");
    let d = t.mock.gh.borrow().dispatches.last().unwrap().clone();
    assert_eq!(
        d,
        json!({"_workflow": "taskset-pack.yml", "taskset_id": id, "source": format!("blob:{zip}"),
               "results_url": format!("https://crucible.example.workers.dev/internal/tasksets/{id}")})
    );

    // Packing: visible to the owner only, not usable yet.
    assert_eq!(
        names(Some(&alice), ""),
        vec!["github-full".to_string(), id.clone()]
    );
    assert_eq!(names(Some(&bob), ""), vec!["github-full".to_string()]);
    assert_eq!(names(None, ""), vec!["github-full".to_string()]);
    let get = |tok: &str| t.as_user("GET", &format!("/tasksets/{id}"), tok, b"");
    assert_eq!(json_of(&get(&alice))["status"], "packing");
    assert_eq!(get(&bob).status, 404);
    let iget = |gid: u64| {
        t.call(
            "GET",
            &format!("/internal/tasksets/{id}?github_id={gid}"),
            &[("Authorization", &internal)],
            b"",
        )
    };
    assert_eq!(iget(42).status, 409);

    // The workflow reports it packed (internal token only, once).
    let result = serde_json::to_vec(&json!({"status": "ready", "taskset": packed(&id)})).unwrap();
    let ipost = |body: &[u8]| {
        t.call(
            "POST",
            &format!("/internal/tasksets/{id}"),
            &[("Authorization", &internal)],
            body,
        )
    };
    assert_eq!(
        t.as_user("POST", &format!("/internal/tasksets/{id}"), &alice, &result)
            .status,
        401
    );
    let wrong_name =
        serde_json::to_vec(&json!({"status": "ready", "taskset": packed("u-0000000000000000")}))
            .unwrap();
    assert_eq!(ipost(&wrong_name).status, 400);
    let mut other_scorer = packed(&id);
    other_scorer["scorer"]["name"] = json!("astro-survey");
    let other_scorer =
        serde_json::to_vec(&json!({"status": "ready", "taskset": other_scorer})).unwrap();
    assert_eq!(ipost(&other_scorer).status, 400);
    assert_eq!(ipost(&result).status, 200);
    assert_eq!(ipost(&result).status, 409);
    let info = json_of(&get(&alice));
    assert_eq!(info["status"], "ready");
    assert_eq!(info["title"], "my-tasks");
    assert_eq!(info["public"], false);
    assert_eq!(
        info["stages"],
        json!([
            {"name": "s1", "time_limit_s": 600, "total": 3},
            {"name": "s2", "time_limit_s": 600, "total": 4}
        ])
    );

    // Private: only Alice may see and use it; the workflow's check agrees.
    assert_eq!(get(&bob).status, 404);
    assert_eq!(names(Some(&bob), ""), vec!["github-full".to_string()]);
    assert_eq!(iget(42).status, 200);
    assert_eq!(json_of(&iget(42))["name"], json!(id));
    assert_eq!(iget(43).status, 403);
    let app_eval = |tok: &str, eid: &str| {
        let kind_hash = up(tok, "app", &sealed(format!("site {eid}").as_bytes()));
        let body = json!({
            "mode": "app", "eval_id": eid, "upload_hash": kind_hash, "taskset": id,
            "stages": 2, "score_public": false, "consent": true
        });
        t.as_user("POST", "/evals", tok, &serde_json::to_vec(&body).unwrap())
    };
    let r = app_eval(&bob, "1b7e6a52-1f3c-4d2a-9e8b-7c6d5e4f3a21");
    assert_eq!(err_code(&r), "bad_request");
    let r = app_eval(&alice, EID);
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let d = t.mock.gh.borrow().dispatches.last().unwrap().clone();
    assert_eq!(
        (d["taskset"].as_str(), d["stage"].as_str()),
        (Some(id.as_str()), Some("2"))
    );
    // A taskset upload is not an eval upload.
    let body = json!({
        "mode": "app", "eval_id": "3b7e6a52-1f3c-4d2a-9e8b-7c6d5e4f3a21", "upload_hash": zip,
        "taskset": id, "stages": 1, "score_public": false, "consent": true
    });
    assert_eq!(
        err_code(&t.as_user(
            "POST",
            "/evals",
            &alice,
            &serde_json::to_vec(&body).unwrap()
        )),
        "bad_request"
    );

    // Admins see every upload with ?all=1 and may make one public.
    assert_eq!(names(Some(&admin), ""), vec!["github-full".to_string()]);
    assert_eq!(
        names(Some(&admin), "?all=1"),
        vec!["github-full".to_string(), id.clone()]
    );
    let public = |tok: &str, on: bool| {
        let body = serde_json::to_vec(&json!({"public": on})).unwrap();
        t.as_user("POST", &format!("/tasksets/{id}/public"), tok, &body)
    };
    assert_eq!(public(&alice, true).status, 403);
    assert_eq!(public(&admin, true).status, 200);
    assert_eq!(names(None, ""), vec!["github-full".to_string(), id.clone()]);
    assert_eq!(get(&bob).status, 200);
    assert_eq!(iget(43).status, 200);
    let r = app_eval(&bob, "4b7e6a52-1f3c-4d2a-9e8b-7c6d5e4f3a21");
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(public(&admin, false).status, 200);
    assert_eq!(iget(43).status, 403);

    // A refused upload: the reason goes to its owner only.
    let zip2 = up(&alice, "taskset", &sealed(b"another zip"));
    let id2 = json_of(&register(&alice, &zip2))["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(id2, id);
    let r = t.call(
        "POST",
        &format!("/internal/tasksets/{id2}"),
        &[("Authorization", &internal)],
        br#"{"status": "failed", "error": "stage s1 tests: tests has no files"}"#,
    );
    assert_eq!(r.status, 200);
    let info = json_of(&t.as_user("GET", &format!("/tasksets/{id2}"), &alice, b""));
    assert_eq!(
        (info["status"].as_str(), info["error"].as_str()),
        (Some("failed"), Some("stage s1 tests: tests has no files"))
    );
    assert_eq!(
        t.as_user(
            "POST",
            &format!("/tasksets/{id2}/public"),
            &admin,
            br#"{"public": true}"#
        )
        .status,
        409
    );
}

fn upload_as(t: &T, tok: &str, body: &[u8]) -> Resp {
    let auth = format!("Bearer {tok}");
    t.call(
        "POST",
        "/uploads",
        &[("Authorization", &auth), ("X-Upload-Kind", "app")],
        body,
    )
}

fn asset_count(t: &T) -> usize {
    t.mock.gh.borrow().assets.values().map(|a| a.len()).sum()
}

/// The upload reached GitHub but its answer was lost, so the client
/// retries the same bytes: the retry succeeds, and only its owner's does.
#[test]
fn upload_retry_after_lost_answer() {
    let t = T::new();
    let alice = token(42, "octocat");
    let bob = token(43, "bob");
    let blob = sealed(b"app ciphertext");
    t.mock.gh.borrow_mut().lose_upload_answer = true;
    let r = upload_as(&t, &alice, &blob);
    assert_eq!(r.status, 502, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(asset_count(&t), 1); // stored nonetheless

    let r = upload_as(&t, &alice, &blob);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(json_of(&r)["hash"], sha256_hex(&blob));
    assert_eq!(upload_as(&t, &alice, &blob).status, 200);
    assert_eq!(asset_count(&t), 1);
    // Someone else's retry of the same bytes is refused.
    assert_eq!(err_code(&upload_as(&t, &bob, &blob)), "conflict");

    // The hash is usable for an eval.
    let eval = json!({
        "mode": "app", "eval_id": EID, "upload_hash": sha256_hex(&blob),
        "taskset": "github-full", "stages": 1, "score_public": false, "consent": true
    });
    let r = t.as_user(
        "POST",
        "/evals",
        &alice,
        &serde_json::to_vec(&eval).unwrap(),
    );
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
}

/// A stored asset that is not what its name says is never accepted.
#[test]
fn upload_retry_checks_stored_bytes() {
    let t = T::new();
    let alice = token(42, "octocat");
    let blob = sealed(b"app ciphertext");
    assert_eq!(upload_as(&t, &alice, &blob).status, 201);
    for list in t.mock.gh.borrow_mut().assets.values_mut() {
        for bytes in list.values_mut() {
            bytes.push(b'!');
        }
    }
    assert_eq!(err_code(&upload_as(&t, &alice, &blob)), "upstream_error");
}

/// Bytes stored without anyone's upload record (e.g. workflow outputs) are
/// not claimable, and the refused attempt leaves no claim behind.
#[test]
fn upload_of_unclaimed_stored_bytes_is_refused() {
    let t = T::new();
    let bob = token(43, "bob");
    let foreign = sealed(b"someone's output");
    let hash = sha256_hex(&foreign);
    {
        let mut gh = t.mock.gh.borrow_mut();
        gh.releases.insert(release_tag(&hash).unwrap(), 777);
        gh.assets
            .entry(777)
            .or_default()
            .insert(hash.clone(), foreign.clone());
    }
    assert_eq!(err_code(&upload_as(&t, &bob, &foreign)), "conflict");
    assert_eq!(
        t.mock.count(&format!(
            "SELECT COUNT(*) FROM uploads WHERE hash = '{hash}'"
        )),
        0
    );
    assert_eq!(err_code(&upload_as(&t, &bob, &foreign)), "conflict");
}

/// Out of writes: the upload fails with an explicit 503 before anything
/// reaches GitHub, so a later retry is a clean first upload.
#[test]
fn upload_when_write_quota_is_exhausted() {
    let t = T::new();
    let alice = token(42, "octocat");
    let blob = sealed(b"app ciphertext");
    // Create the release first (its cache write is best-effort anyway).
    assert_eq!(upload_as(&t, &alice, &sealed(b"warm-up")).status, 201);
    let before = asset_count(&t);
    *t.mock.write_error.borrow_mut() =
        Some("D1_ERROR: Exceeded maximum daily rows written limit".into());
    let r = upload_as(&t, &alice, &blob);
    assert_eq!(r.status, 503);
    assert_eq!(err_code(&r), "storage_quota");
    assert_eq!(asset_count(&t), before);
    *t.mock.write_error.borrow_mut() = None;
    assert_eq!(upload_as(&t, &alice, &blob).status, 201);
}

/// The same results posted again (an answer was lost, or a rerun posts
/// what is stored) succeed without writing anything, even with the write
/// quota gone; new results then report the quota explicitly.
#[test]
fn results_repeated_delivery() {
    let t = T::new();
    let alice = token(42, "octocat");
    submit_app(&t, &alice);
    let status = |s: &str| {
        t.as_user(
            "POST",
            &format!("/internal/status/{EID}"),
            WORKER_TOKEN,
            &serde_json::to_vec(&json!({"status": s})).unwrap(),
        )
    };
    assert_eq!(status("scoring").status, 200);
    let before = t.mock.writes();
    assert_eq!(status("scoring").status, 200);
    assert_eq!(t.mock.writes(), before, "a repeated status writes nothing");

    let mut manifest = app_manifest(1);
    manifest["download"] = json!({"sha256": "ab".repeat(32)});
    let post = |m: &Value| {
        t.as_user(
            "POST",
            &format!("/internal/results/{EID}"),
            WORKER_TOKEN,
            &serde_json::to_vec(m).unwrap(),
        )
    };
    let r = post(&manifest);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let detail = json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(detail["status"], "done");
    assert_eq!(detail["download_available"], true);

    t.mock.now.set(NOW + 30);
    let before = t.mock.writes();
    let r = post(&manifest);
    assert_eq!(
        (r.status, json_of(&r)["status"].as_str()),
        (200, Some("done"))
    );
    assert_eq!(t.mock.writes(), before);
    assert_eq!(
        t.mock.count(&format!(
            "SELECT COUNT(*) FROM results WHERE updated_at = '{}'",
            util::rfc3339(NOW)
        )),
        1
    );

    // With the quota gone, a repeat still succeeds (nothing to write)...
    *t.mock.write_error.borrow_mut() =
        Some("D1_ERROR: Exceeded maximum daily rows written limit".into());
    assert_eq!(post(&manifest).status, 200);
    // ...while new results report the quota explicitly.
    let mut changed = manifest.clone();
    changed["replicas"][0]["stages"][0]["score"]["passed"] = json!(0);
    let r = post(&changed);
    assert_eq!((r.status, err_code(&r).as_str()), (503, "storage_quota"));
}

/// Writes of a whole agent eval (2 stages): the upload claim, the eval row,
/// one row per stored progress report and the results row in D1; the
/// credential put and the workflow's delete in KV. Queries write nothing.
#[test]
fn writes_per_eval() {
    let t = T::new();
    let alice = token(42, "octocat");
    // Warm the GitHub caches (release, taskset list) first.
    assert_eq!(upload_as(&t, &alice, &sealed(b"warm-up")).status, 201);
    assert_eq!(t.call("GET", "/tasksets", &[], b"").status, 200);
    let start = t.mock.writes();

    let auth = format!("Bearer {alice}");
    let r = t.call(
        "POST",
        "/uploads",
        &[("Authorization", &auth), ("X-Upload-Kind", "agent")],
        &sealed(b"agent"),
    );
    let hash = json_of(&r)["hash"].as_str().unwrap().to_string();
    let eval = json!({
        "mode": "agent", "eval_id": EID, "upload_hash": hash, "taskset": "github-full",
        "model": "m", "cred_envelope": b64_encode(&sealed(b"k")), "score_public": false,
        "consent": true
    });
    let r = t.as_user(
        "POST",
        "/evals",
        &alice,
        &serde_json::to_vec(&eval).unwrap(),
    );
    assert_eq!(r.status, 201);
    let detail = || {
        let r = t.as_user("GET", &format!("/evals/{EID}"), &alice, b"");
        assert_eq!(r.status, 200);
        json_of(&r)
    };
    let report = |s: &str| {
        let r = t.as_user(
            "POST",
            &format!("/internal/status/{EID}"),
            WORKER_TOKEN,
            &serde_json::to_vec(&json!({"status": s})).unwrap(),
        );
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    };
    detail();
    t.mock.gh.borrow_mut().runs[0].2 = "in_progress".into();
    assert_eq!(
        t.as_user("GET", &format!("/internal/cred/{EID}"), WORKER_TOKEN, b"")
            .status,
        200
    );
    report("building");
    assert_eq!(detail()["status"], "running:stage-1"); // the GitHub estimate
    report("running:stage-1");
    report("running:stage-1");
    detail();
    report("running:stage-2");
    assert_eq!(detail()["status"], "running:stage-2");
    report("scoring");
    detail();
    t.as_user("GET", "/evals", &alice, b"");
    let mut m = app_manifest(1);
    m["model"] = json!("m");
    let r = t.as_user(
        "POST",
        &format!("/internal/results/{EID}"),
        WORKER_TOKEN,
        &serde_json::to_vec(&m).unwrap(),
    );
    assert_eq!(json_of(&r)["status"], "done");
    assert_eq!(
        t.as_user(
            "DELETE",
            &format!("/internal/cred/{EID}"),
            WORKER_TOKEN,
            b""
        )
        .status,
        204
    );
    let d = detail();
    assert_eq!(d["status"], "done");
    assert!(d["run_url"].as_str().unwrap().ends_with("/runs/9000"));
    t.mock.gh.borrow_mut().runs[0].2 = "completed".into();
    t.mock.gh.borrow_mut().runs[0].3 = Some("success".into());
    detail();

    // upload + eval + credential put + running:stage-1 + running:stage-2
    // + scoring + results + credential delete (the results' own cleanup
    // finds it gone and writes nothing)
    assert_eq!(t.mock.writes() - start, 8);
}

/// Submits an app-mode eval of stage 1 and posts a done result whose
/// total is `passed`/2 (agent `agent`). It counts as having run every stage
/// of github-full (as an agent eval would), so it goes on the main board.
fn scored_eval(t: &T, tok: &str, id: &str, public: bool, agent: &str, passed: u32) {
    partial_eval(t, tok, id, public, agent, passed);
    t.mock.sql(&format!(
        "UPDATE evals SET stage_names = '[\"stage-1\",\"stage-2\"]' WHERE eval_id = '{id}'"
    ));
}

/// Like [`scored_eval`], but only stage 1 of github-full's 2 stages ran.
fn partial_eval(t: &T, tok: &str, id: &str, public: bool, agent: &str, passed: u32) {
    let hash = json_of(&upload_as(t, tok, &sealed(id.as_bytes())))["hash"]
        .as_str()
        .unwrap()
        .to_string();
    let eval = json!({
        "mode": "app", "eval_id": id, "upload_hash": hash, "taskset": "github-full",
        "stages": 1, "score_public": public, "consent": true
    });
    let r = t.as_user("POST", "/evals", tok, &serde_json::to_vec(&eval).unwrap());
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let mut m = app_manifest(passed);
    m["eval_id"] = json!(id);
    m["agent"]["name"] = json!(agent);
    m["replicas"][0]["stages"][0]["score"]["total"] = json!(2);
    let r = t.as_user(
        "POST",
        &format!("/internal/results/{id}"),
        WORKER_TOKEN,
        &serde_json::to_vec(&m).unwrap(),
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    t.mock.now.set(t.mock.now.get() + 10);
}

#[test]
fn leaderboard() {
    let t = T::new();
    let alice = token(42, "octocat");
    let bob = token(43, "bob");
    let id = |n: u8| format!("{n:08x}-1f3c-4d2a-9e8b-7c6d5e4f3a21");
    scored_eval(&t, &alice, &id(1), true, "x", 1);
    scored_eval(&t, &alice, &id(2), true, "x", 2); // alice's best x
    scored_eval(&t, &alice, &id(3), false, "x", 2); // private: never shown
    scored_eval(&t, &bob, &id(4), false, "y", 2); // private
    scored_eval(&t, &bob, &id(5), true, "y", 1);
    // Anonymous.
    let r = t.call("GET", "/leaderboard/github-full", &[], b"");
    assert_eq!(r.status, 200);
    let b = json_of(&r);
    assert_eq!(b["direction"], "higher");
    let rows: Vec<(u64, &str, &str, f64)> = b["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["rank"].as_u64().unwrap(),
                e["login"].as_str().unwrap(),
                e["eval_id"].as_str().unwrap(),
                e["total_score"].as_f64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (1, "octocat", id(2).as_str(), 1.0),
            (2, "bob", id(5).as_str(), 0.5)
        ]
    );
    let e = &b["entries"][0];
    assert_eq!(e["agent"], "x");
    assert_eq!(
        e["stages"],
        json!([{"stage": "stage-1", "score": 2.0, "max": 2.0}])
    );
    assert_eq!(e["replicas"], 1);
    let text = String::from_utf8_lossy(&r.body);
    assert!(!text.contains(&id(3)) && !text.contains(&id(4)));
    assert_eq!(
        json_of(&t.call("GET", "/leaderboard", &[], b"")),
        json!([{"taskset": "github-full", "evals": 3, "latest_at": b["entries"][1]["created_at"]}])
    );
    // Cached for 5 minutes.
    scored_eval(&t, &bob, &id(6), true, "y", 2);
    assert_eq!(
        json_of(&t.call("GET", "/leaderboard/github-full", &[], b"")),
        b
    );
    t.mock.now.set(t.mock.now.get() + 301);
    let b = json_of(&t.call("GET", "/leaderboard/github-full", &[], b""));
    assert_eq!(b["entries"][1]["rank"], 1); // tied with alice, later
    assert_eq!(b["entries"][1]["eval_id"], id(6).as_str());
    // A banned owner drops out.
    t.mock
        .sql("INSERT INTO bans VALUES (43, 1, 'now', NULL); DELETE FROM cache;");
    let b = json_of(&t.call("GET", "/leaderboard/github-full", &[], b""));
    assert_eq!(b["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        t.call("GET", "/leaderboard/Bad..Name", &[], b"").status,
        404
    );
    assert_eq!(
        json_of(&t.call("GET", "/leaderboard/nothing-here", &[], b""))["entries"],
        json!([])
    );

    // Evals of only some stages are ranked apart, by the stages they ran.
    partial_eval(&t, &alice, &id(7), true, "z", 2);
    t.mock.sql("DELETE FROM cache;");
    let b = json_of(&t.call("GET", "/leaderboard/github-full", &[], b""));
    assert_eq!(b["entries"].as_array().unwrap().len(), 1);
    assert_eq!(b["partial"].as_array().unwrap().len(), 1);
    assert_eq!(b["partial"][0]["stages"], json!(["stage-1"]));
    assert_eq!(b["partial"][0]["entries"][0]["eval_id"], id(7).as_str());
    assert_eq!(b["partial"][0]["entries"][0]["rank"], 1);
    // The eval itself says it is partial.
    let d = json_of(&t.as_user("GET", &format!("/evals/{}", id(7)), &alice, b""));
    assert_eq!(d["complete"], false);
    let d = json_of(&t.as_user("GET", &format!("/evals/{}", id(2)), &alice, b""));
    assert_eq!(d["complete"], true);
}

#[test]
fn user_plugins() {
    let t = T::new();
    let alice = token(42, "octocat");
    let bob = token(43, "bob");
    let admin = token(1, "admin");
    let internal = format!("Bearer {WORKER_TOKEN}");
    let up = |tok: &str, kind: &str, body: &[u8]| {
        let r = t.call(
            "POST",
            "/uploads",
            &[
                ("Authorization", &format!("Bearer {tok}")),
                ("X-Upload-Kind", kind),
            ],
            body,
        );
        assert!(r.status < 300, "{}", String::from_utf8_lossy(&r.body));
        json_of(&r)["hash"].as_str().unwrap().to_string()
    };
    let register = |path: &str, tok: &str, hash: &str| {
        let body = serde_json::to_vec(&json!({"upload_hash": hash})).unwrap();
        let r = t.as_user("POST", path, tok, &body);
        assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
        json_of(&r)["id"].as_str().unwrap().to_string()
    };
    let ipost = |path: String, body: Value| {
        t.call(
            "POST",
            &path,
            &[("Authorization", &internal)],
            &serde_json::to_vec(&body).unwrap(),
        )
    };
    let iget = |path: String| t.call("GET", &path, &[("Authorization", &internal)], b"");

    // Alice uploads a plugin; only a plugin upload registers.
    let pkg = up(&alice, "plugin", &sealed(b"plugin zip"));
    let other = up(&alice, "taskset", &sealed(b"a taskset"));
    let r = t.as_user(
        "POST",
        "/plugins",
        &alice,
        &serde_json::to_vec(&json!({"upload_hash": other})).unwrap(),
    );
    assert_eq!(err_code(&r), "bad_request");
    let pid = register("/plugins", &alice, &pkg);
    assert!(crucible_core::plugins::is_user_plugin_id(&pid));
    let d = t.mock.gh.borrow().dispatches.last().unwrap().clone();
    assert_eq!(
        d,
        json!({"_workflow": "plugin-pack.yml", "plugin_id": pid, "source": format!("blob:{pkg}"),
               "results_url": format!("https://crucible.example.workers.dev/internal/plugins/{pid}")})
    );
    assert_eq!(
        json_of(&t.as_user("GET", &format!("/plugins/{pid}"), &alice, b""))["status"],
        "building"
    );
    assert_eq!(
        t.as_user("GET", &format!("/plugins/{pid}"), &bob, b"")
            .status,
        404
    );

    // The workflow reports it built.
    let pinned = json!({"kind": "scorer", "name": pid, "version": "0.1",
        "blob": {"sha256": pkg, "key_id": "1ffa702796eb5ee8"},
        "runs_taskset_code": false, "model": false, "accepts": ["files"]});
    let mut wrong = pinned.clone();
    wrong["blob"]["sha256"] = json!("cd".repeat(32));
    let ready = |p: &Value| {
        json!({"status": "ready", "plugin": p, "title": "keyword", "description": "d",
        "selftest": {"status": "scored", "score": 1.0, "max": 2.0, "detail": "ok"},
        "review": {"files": [{"path": "Dockerfile", "size": 40}, {"path": "score.py", "size": 12}],
                   "dockerfile": "FROM python:3.12-slim\nCOPY score.py /s.py",
                   "texts": [{"path": "score.py", "content": "print('<b>')"}]}})
    };
    assert_eq!(
        ipost(format!("/internal/plugins/{pid}"), ready(&wrong)).status,
        400
    );
    assert_eq!(
        ipost(format!("/internal/plugins/{pid}"), ready(&pinned)).status,
        200
    );
    assert_eq!(
        ipost(format!("/internal/plugins/{pid}"), ready(&pinned)).status,
        409
    );
    let v = json_of(&t.as_user("GET", &format!("/plugins/{pid}"), &alice, b""));
    assert_eq!(
        (
            v["status"].as_str(),
            v["title"].as_str(),
            v["version"].as_str()
        ),
        (Some("ready"), Some("keyword"), Some("0.1"))
    );
    assert_eq!(v["selftest"]["score"], 1.0);
    let listed = |tok: &str| -> Vec<String> {
        json_of(&t.as_user("GET", "/plugins", tok, b""))
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(listed(&alice), vec![pid.clone()]);
    assert!(listed(&bob).is_empty());

    // Tasksets: Alice's may use her private plugin, Bob's may not.
    let ts_a = register(
        "/tasksets",
        &alice,
        &up(&alice, "taskset", &sealed(b"ts a")),
    );
    let ts_b = register("/tasksets", &bob, &up(&bob, "taskset", &sealed(b"ts b")));
    let r = iget(format!("/internal/plugins/{pid}?taskset={ts_a}"));
    assert_eq!(r.status, 200);
    assert_eq!(json_of(&r), pinned);
    assert_eq!(
        iget(format!("/internal/plugins/{pid}?taskset={ts_b}")).status,
        403
    );
    let with_plugin = |id: &str| {
        let mut ts = packed(id);
        ts["scorer"] = json!({"name": pid});
        for s in ts["stages"].as_array_mut().unwrap() {
            s["output"] = json!("files");
            s.as_object_mut().unwrap().remove("expected_total");
        }
        ts["user_plugins"] = json!([pinned]);
        json!({"status": "ready", "taskset": ts})
    };
    // Bob's taskset naming Alice's private plugin is refused even if the
    // workflow said ready.
    assert_eq!(
        ipost(format!("/internal/tasksets/{ts_b}"), with_plugin(&ts_b)).status,
        400
    );
    assert_eq!(
        ipost(format!("/internal/tasksets/{ts_a}"), with_plugin(&ts_a)).status,
        200
    );

    // The review material: the uploader and admins, nobody else.
    let rv = t.as_user("GET", &format!("/plugins/{pid}/review"), &alice, b"");
    assert_eq!(rv.status, 200);
    let rv = json_of(&rv);
    assert_eq!(rv["review"]["texts"][0]["content"], "print('<b>')");
    assert_eq!(
        rv["review"]["dockerfile"],
        "FROM python:3.12-slim\nCOPY score.py /s.py"
    );
    assert_eq!(rv["checklist"].as_array().unwrap().len(), 4);
    assert_eq!(
        t.as_user("GET", &format!("/plugins/{pid}/review"), &bob, b"")
            .status,
        404
    );
    assert_eq!(
        t.as_user("GET", &format!("/plugins/{pid}/review"), &admin, b"")
            .status,
        200
    );

    // Public: anyone's tasksets may use it, after the review checklist.
    let reviewed = br#"{"public": true, "review": {"checked": ["source", "dockerfile", "detail_leak", "model_use"], "note": "read it"}}"#;
    let public = |tok: &str| t.as_user("POST", &format!("/plugins/{pid}/public"), tok, reviewed);
    assert_eq!(public(&alice).status, 403);
    let r = t.as_user(
        "POST",
        &format!("/plugins/{pid}/public"),
        &admin,
        br#"{"public": true}"#,
    );
    assert_eq!(r.status, 400, "no review");
    let r = t.as_user(
        "POST",
        &format!("/plugins/{pid}/public"),
        &admin,
        br#"{"public": true, "review": {"checked": ["source", "dockerfile"]}}"#,
    );
    assert_eq!(r.status, 400);
    assert!(
        json_of(&r)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("detail_leak, model_use")
    );
    assert_eq!(public(&admin).status, 200);
    let v = json_of(&t.as_user("GET", &format!("/plugins/{pid}"), &bob, b""));
    assert_eq!(
        (
            v["approval"]["by_id"].as_u64(),
            v["approval"]["note"].as_str()
        ),
        (Some(1), Some("read it"))
    );
    // Making it private needs no review; the approval stays on record.
    assert_eq!(
        t.as_user(
            "POST",
            &format!("/plugins/{pid}/public"),
            &admin,
            br#"{"public": false}"#
        )
        .status,
        200
    );
    assert_eq!(listed(&bob), Vec::<String>::new());
    assert_eq!(public(&admin).status, 200);
    assert_eq!(listed(&bob), vec![pid.clone()]);
    assert_eq!(
        iget(format!("/internal/plugins/{pid}?taskset={ts_b}")).status,
        200
    );
    assert_eq!(
        ipost(format!("/internal/tasksets/{ts_b}"), with_plugin(&ts_b)).status,
        200
    );

    // A refused package: the reason goes to its owner.
    let pid2 = register("/plugins", &alice, &up(&alice, "plugin", &sealed(b"bad")));
    let r = ipost(
        format!("/internal/plugins/{pid2}"),
        json!({"status": "failed", "error": "no Dockerfile"}),
    );
    assert_eq!(r.status, 200);
    let v = json_of(&t.as_user("GET", &format!("/plugins/{pid2}"), &alice, b""));
    assert_eq!(
        (v["status"].as_str(), v["error"].as_str()),
        (Some("failed"), Some("no Dockerfile"))
    );
    assert_eq!(public(&admin).status, 200);
    assert_eq!(
        t.as_user("POST", &format!("/plugins/{pid2}/public"), &admin, reviewed)
            .status,
        409
    );
}

#[test]
fn quotas() {
    let t = T::new();
    let alice = token(42, "octocat");
    let admin = token(1, "admin");
    let id = |n: u8| format!("{n:08x}-1f3c-4d2a-9e8b-7c6d5e4f3a21");
    let q = |tok: &str| json_of(&t.as_user("GET", "/quota", tok, b""));
    let item = |v: &Value, name: &str| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == name)
            .unwrap()
            .clone()
    };
    let v = q(&alice);
    assert_eq!(v["exempt"], false);
    assert_eq!(item(&v, "evals_running")["limit"], 3);
    assert_eq!(item(&v, "evals_running")["remaining"], 3);
    assert_eq!(q(&admin)["exempt"], true);

    // Only admins (by session) set overrides.
    let set = |tok: &str, gid: u64, body: &str| {
        t.as_user("PUT", &format!("/admin/quotas/{gid}"), tok, body.as_bytes())
    };
    assert_eq!(set(&alice, 42, "{}").status, 403);
    assert_eq!(
        set(&admin, 42, r#"{"evals_running": 1, "bogus": 1}"#).status,
        400
    );
    let r = set(&admin, 42, r#"{"evals_running": 1, "evals_per_day": 2}"#);
    assert_eq!(r.status, 200);
    assert_eq!(item(&json_of(&r), "evals_running")["limit"], 1);

    let submit = |n: u8| {
        let hash = json_of(&upload_as(&t, &alice, &sealed(&[b's', n])))["hash"]
            .as_str()
            .unwrap()
            .to_string();
        let eval = json!({
            "mode": "app", "eval_id": id(n), "upload_hash": hash, "taskset": "github-full",
            "stages": 1, "score_public": false, "consent": true
        });
        t.as_user(
            "POST",
            "/evals",
            &alice,
            &serde_json::to_vec(&eval).unwrap(),
        )
    };
    assert_eq!(submit(1).status, 201);
    // One in progress: the second waits.
    let r = submit(2);
    assert_eq!((r.status, err_code(&r).as_str()), (429, "quota_exceeded"));
    let msg = json_of(&r)["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        msg.contains("evals_running") && msg.contains("limit 1"),
        "{msg}"
    );
    assert_eq!(item(&q(&alice), "evals_running")["remaining"], 0);
    // Settled: the next may start; then the daily limit (2) is reached.
    let r = t.as_user(
        "POST",
        &format!("/internal/status/{}", id(1)),
        WORKER_TOKEN,
        br#"{"status": "failed"}"#,
    );
    assert_eq!(r.status, 200);
    assert_eq!(submit(3).status, 201);
    t.as_user(
        "POST",
        &format!("/internal/status/{}", id(3)),
        WORKER_TOKEN,
        br#"{"status": "failed"}"#,
    );
    let r = submit(4);
    assert_eq!(r.status, 429);
    let msg = json_of(&r)["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        msg.contains("evals_per_day") && msg.contains("2026-10-04T00:00:00Z"),
        "{msg}"
    );
    // A day later the window has moved on.
    t.mock.now.set(NOW + 86_400);
    let alice = issue_session(SESSION_KEY.as_bytes(), 42, "octocat", NOW + 86_400);
    let admin = issue_session(SESSION_KEY.as_bytes(), 1, "admin", NOW + 86_400);
    assert_eq!(item(&q(&alice), "evals_per_day")["used"], 0);

    // Uploads: count and bytes; a retry of one's own upload is not counted.
    set(
        &admin,
        42,
        r#"{"uploads_per_day": 1, "upload_bytes_per_day": 100000}"#,
    );
    let body = sealed(b"one");
    assert_eq!(upload_as(&t, &alice, &body).status, 201);
    assert_eq!(upload_as(&t, &alice, &body).status, 200);
    let r = upload_as(&t, &alice, &sealed(b"two"));
    assert_eq!(r.status, 429);
    assert!(
        json_of(&r)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("uploads_per_day")
    );
    set(&admin, 42, r#"{"upload_bytes_per_day": 10}"#);
    let r = upload_as(&t, &alice, &sealed(b"three"));
    assert!(
        json_of(&r)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upload_bytes_per_day")
    );
    // Exempt; and an admin can be made subject to quotas.
    set(
        &admin,
        42,
        r#"{"upload_bytes_per_day": 10, "exempt": true}"#,
    );
    assert_eq!(upload_as(&t, &alice, &sealed(b"four")).status, 201);
    set(&admin, 1, r#"{"plugins_per_day": 0, "exempt": false}"#);
    let v = json_of(&t.as_user("GET", "/admin/quotas/1", &admin, b""));
    assert_eq!(
        (
            v["exempt"].as_bool(),
            v["override"]["plugins_per_day"].as_u64()
        ),
        (Some(false), Some(0))
    );
    let auth = format!("Bearer {admin}");
    let r = t.call(
        "POST",
        "/uploads",
        &[("Authorization", &auth), ("X-Upload-Kind", "plugin")],
        &sealed(b"plugin"),
    );
    let hash = json_of(&r)["hash"].as_str().unwrap().to_string();
    let r = t.as_user(
        "POST",
        "/plugins",
        &admin,
        &serde_json::to_vec(&json!({"upload_hash": hash})).unwrap(),
    );
    assert_eq!(r.status, 429);
    assert!(
        json_of(&r)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("plugins_per_day")
    );
    // `{}` removes the override.
    assert_eq!(set(&admin, 1, "{}").status, 200);
    assert_eq!(
        t.mock
            .count("SELECT COUNT(*) FROM quotas WHERE github_id = 1"),
        0
    );
}

#[test]
fn sweep_fails_stuck_records() {
    let t = T::new();
    let alice = token(42, "octocat");
    let cron = || block_on(crucible_worker::app::scheduled(&t.mock, Some(&t.cfg)));
    let up = |kind: &str, body: &[u8]| {
        let auth = format!("Bearer {alice}");
        let r = t.call(
            "POST",
            "/uploads",
            &[("Authorization", &auth), ("X-Upload-Kind", kind)],
            body,
        );
        json_of(&r)["hash"].as_str().unwrap().to_string()
    };
    let reg = |path: &str, hash: &str| {
        let r = t.as_user(
            "POST",
            path,
            &alice,
            &serde_json::to_vec(&json!({"upload_hash": hash})).unwrap(),
        );
        json_of(&r)["id"].as_str().unwrap().to_string()
    };
    let ts = reg("/tasksets", &up("taskset", &sealed(b"ts")));
    let pl = reg("/plugins", &up("plugin", &sealed(b"pl")));
    submit_app(&t, &alice); // EID, its run stays queued
    // Within the limits nothing changes.
    t.mock.now.set(NOW + 3600);
    cron();
    assert_eq!(
        t.mock
            .count("SELECT COUNT(*) FROM user_plugins WHERE status = 'building'"),
        1
    );
    // Past 2 h: both registrations fail with a reason.
    t.mock.now.set(NOW + 2 * 3600 + 1);
    cron();
    let v = json_of(&t.as_user("GET", &format!("/plugins/{pl}"), &alice, b""));
    assert_eq!(v["status"], "failed");
    assert!(v["error"].as_str().unwrap().contains("building timed out"));
    let v = json_of(&t.as_user("GET", &format!("/tasksets/{ts}"), &alice, b""));
    assert_eq!(v["status"], "failed");
    assert!(v["error"].as_str().unwrap().contains("packing timed out"));
    // The eval has a run (queued on GitHub): kept until the absolute limit.
    let detail = || json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(detail()["status"], "queued");
    t.mock.now.set(NOW + 30 * 3600 + 1);
    cron();
    let alice = issue_session(SESSION_KEY.as_bytes(), 42, "octocat", NOW + 30 * 3600);
    let d = json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    assert_eq!(d["status"], "failed");
    assert!(
        d["error"].as_str().unwrap().contains("no result 30 h"),
        "{d}"
    );
}

#[test]
fn sweep_settles_finished_runs() {
    let t = T::new();
    let alice = token(42, "octocat");
    submit_app(&t, &alice);
    t.mock.gh.borrow_mut().runs[0].2 = "completed".into();
    t.mock.gh.borrow_mut().runs[0].3 = Some("failure".into());
    t.mock.now.set(NOW + 3601);
    block_on(crucible_worker::app::scheduled(&t.mock, Some(&t.cfg)));
    assert_eq!(
        t.mock.count(&format!(
            "SELECT COUNT(*) FROM evals WHERE eval_id = '{EID}' AND status = 'failed'"
        )),
        1
    );
}
