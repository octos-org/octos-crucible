//! End-to-end flow through the real router with an in-memory KV and a fake
//! GitHub: login → upload → submit → Actions fetches and deletes the
//! credential → results → query → download, plus the refusal paths.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use crucible_core::Envelope;
use crucible_worker::app::handle;
use crucible_worker::config::Config;
use crucible_worker::http::{Backend, HttpRequest, HttpResponse, KvKey, PutOptions, Req, Resp};
use crucible_worker::session::issue_session;
use crucible_worker::shard::{release_tag, sha256_hex};
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
    assets: BTreeMap<u64, Vec<String>>,
    dispatches: Vec<Value>,
    /// (run id, eval id, status, conclusion)
    runs: Vec<(u64, String, String, Option<String>)>,
}

/// value, metadata, expiry (unix seconds)
type KvEntry = (Vec<u8>, Option<Value>, Option<u64>);

struct Mock {
    kv: RefCell<BTreeMap<String, KvEntry>>,
    gh: RefCell<FakeGitHub>,
    now: Cell<u64>,
    logs: RefCell<Vec<String>>,
    rng: Cell<u8>,
    /// Keys whose reads return an old value (KV is eventually consistent:
    /// a read can miss a recent write). See [`Mock::freeze`].
    stale: RefCell<BTreeMap<String, Option<Vec<u8>>>>,
}

impl Mock {
    fn new() -> Mock {
        Mock {
            kv: RefCell::default(),
            gh: RefCell::default(),
            now: Cell::new(NOW),
            logs: RefCell::default(),
            rng: Cell::new(7),
            stale: RefCell::default(),
        }
    }

    /// Until [`Mock::thaw`], reads of `key` return its current value, while
    /// writes still land.
    fn freeze(&self, key: &str) {
        let v = self.kv.borrow().get(key).map(|(v, _, _)| v.clone());
        self.stale.borrow_mut().insert(key.into(), v);
    }

    fn thaw(&self, key: &str) {
        self.stale.borrow_mut().remove(key);
    }

    fn github(&self, r: &HttpRequest) -> HttpResponse {
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
            if list.contains(&name.1.to_string()) {
                return ok(422, json!({"errors": [{"code": "already_exists"}]}));
            }
            list.push(name.1.to_string());
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
            ("POST", "actions/workflows/taskset-pack.yml/dispatches") => {
                let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
                let mut inputs = body["inputs"].clone();
                inputs["_workflow"] = json!("taskset-pack.yml");
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

impl Backend for Mock {
    async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(old) = self.stale.borrow().get(key) {
            return Ok(old.clone());
        }
        let now = self.now.get();
        Ok(self
            .kv
            .borrow()
            .get(key)
            .filter(|(_, _, exp)| exp.is_none_or(|e| e > now))
            .map(|(v, _, _)| v.clone()))
    }
    async fn kv_put(&self, key: &str, value: &[u8], opts: PutOptions) -> Result<(), String> {
        assert!(opts.ttl.is_none_or(|t| t >= 60), "KV TTL minimum is 60s");
        if let Some(m) = &opts.metadata {
            assert!(
                serde_json::to_vec(m).unwrap().len() <= 1024,
                "KV metadata limit"
            );
        }
        let exp = opts.ttl.map(|t| self.now.get() + t);
        self.kv
            .borrow_mut()
            .insert(key.into(), (value.to_vec(), opts.metadata, exp));
        Ok(())
    }
    async fn kv_delete(&self, key: &str) -> Result<(), String> {
        self.kv.borrow_mut().remove(key);
        Ok(())
    }
    async fn kv_list(&self, prefix: &str, limit: usize) -> Result<Vec<KvKey>, String> {
        Ok(self
            .kv
            .borrow()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .take(limit)
            .map(|(k, (_, m, _))| KvKey {
                name: k.clone(),
                metadata: m.clone(),
            })
            .collect())
    }
    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, String> {
        Ok(self.github(&req))
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
    block_on(t.mock.kv_put("ban/42", b"{}", PutOptions::default())).unwrap();
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
        gh.assets.entry(id).or_default().push(sha256_hex(&foreign));
    }
    block_on(t.mock.kv_delete(&format!(
        "cache/release/{}",
        release_tag(&sha256_hex(&foreign)).unwrap()
    )))
    .unwrap();
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
            "stage": "2", "cred_source": "none", "score_public": "false",
            "owner": "42:octocat",
            "results_url": format!("https://crucible.example.workers.dev/internal/results/{EID}")
        })
    );
    assert!(
        block_on(t.mock.kv_get(&format!("cred/{EID}")))
            .unwrap()
            .is_none()
    );
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

/// A refresh that read the record before the results were written, and
/// wrote it back after, must not lose them; nor may reads that miss the
/// results write for a while (KV is eventually consistent).
#[test]
fn results_survive_stale_refresh() {
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
    let r = t.as_user(
        "POST",
        "/evals",
        &alice,
        &serde_json::to_vec(&eval).unwrap(),
    );
    assert_eq!(r.status, 201);
    let detail = || json_of(&t.as_user("GET", &format!("/evals/{EID}"), &alice, b""));
    t.mock.gh.borrow_mut().runs[0].2 = "in_progress".into();
    assert_eq!(detail()["status"], "running:stage-1");

    // Reads of the record and the results now miss later writes.
    let (rec_key, res_key) = (format!("evals/{EID}"), format!("results/{EID}"));
    t.mock.freeze(&rec_key);
    t.mock.freeze(&res_key);
    t.mock.gh.borrow_mut().runs[0].2 = "completed".into();
    t.mock.gh.borrow_mut().runs[0].3 = Some("success".into());
    let manifest = json!({
        "schema": 1, "eval_id": EID, "created_at": "2026-10-03T00:00:00Z",
        "taskset": "github-full", "agent": {"name": "uploaded", "version": "1"},
        "model": "app", "public": false,
        "replicas": [{"replica": 1, "stages": [
            {"stage": "stage-1", "score": {"status": "passed", "passed": 1, "total": 1},
             "usage": {"requests": 0, "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0, "reasoning_tokens": 0}}
        ]}]
    });
    let r = t.as_user(
        "POST",
        &format!("/internal/results/{EID}"),
        WORKER_TOKEN,
        &serde_json::to_vec(&manifest).unwrap(),
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    // This refresh read the record from before the results and writes it
    // back after them.
    assert_ne!(detail()["status"], "done");
    // Long enough later, a refresh that still misses the results gives up
    // on them and writes `failed`.
    t.mock.thaw(&rec_key);
    t.mock.now.set(NOW + 601);
    assert_eq!(detail()["status"], "failed");

    // Once the results are visible, they win, and the list follows.
    t.mock.thaw(&res_key);
    let r = detail();
    assert_eq!(r["status"], "done");
    assert_eq!(r["total_score"], 1.0);
    assert_eq!(
        r["manifest"]["replicas"][0]["stages"][0]["score"]["passed"],
        1
    );
    let list = json_of(&t.as_user("GET", "/evals", &alice, b""));
    assert_eq!(list[0]["status"], "done");
    assert_eq!(list[0]["total_score"], 1.0);
    // A late refresh from a stale record cannot undo it either.
    t.mock.now.set(NOW + 2000);
    assert_eq!(detail()["status"], "done");
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
    t.mock.now.set(NOW + 86_400);
    assert_eq!(t.as_user("GET", &path, WORKER_TOKEN, b"").status, 404);
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
    // KV holds only the hash.
    let stored: Vec<u8> = t
        .mock
        .kv
        .borrow()
        .values()
        .flat_map(|(v, _, _)| v.clone())
        .collect();
    assert!(!String::from_utf8_lossy(&stored).contains(&cli[20..]));
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
