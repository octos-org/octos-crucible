//! `crucible submit` / `crucible status`: the website's submit flow from
//! the command line (docs/guide/README.md, 命令行提交).
//!
//! Same bytes as the page (web/src/submit.ts): the zip is sealed to the
//! platform key, `POST /uploads`; the credential `{api_key, endpoint,
//! download_password, eval_id}` is sealed and base64-encoded into
//! `POST /evals`. Auth is a personal API token from `CRUCIBLE_TOKEN`.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use crucible_core::netpolicy::validate_endpoint;
use crucible_crypto::PublicKey;
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde_json::{Value, json};

use crate::cred::base64_encode;
use crate::zipdir;

pub const DEFAULT_API: &str = "https://crucible-worker.stratosphericus.workers.dev";
pub const TOKEN_ENV: &str = "CRUCIBLE_TOKEN";
/// `POST /uploads` limit on the sealed bytes, and the page's margin for age.
pub const MAX_UPLOAD: usize = 25 * 1024 * 1024;
pub const MAX_PLAIN: usize = MAX_UPLOAD - 64 * 1024;
pub const MIN_PASSWORD: usize = 12;
pub const MAX_REPLICAS: u32 = 10;
/// Verbatim from web/src/validate.ts (`CONSENT_TEXT`).
pub const CONSENT_TEXT: &str = "你上传的内容、评测产出和日志会加密后永久保存，我们会用于研究和改进平台。你的模型 key 和下载密码在评测结束后立即删除，不会保存。";
const BUNDLED_KEYS: &str = include_str!("../../../config/keys.json");

#[derive(clap::Args)]
pub struct ApiArgs {
    /// Worker address.
    #[arg(long, env = "CRUCIBLE_API", default_value = DEFAULT_API)]
    pub api: String,
}

#[derive(clap::Subcommand)]
pub enum SubmitCmd {
    /// Full evaluation of an agent package.
    Agent(Box<AgentArgs>),
    /// Score an already generated output zip for one stage.
    App(AppArgs),
}

#[derive(clap::Args)]
pub struct Common {
    #[command(flatten)]
    pub api: ApiArgs,
    /// Taskset name (GET /tasksets).
    #[arg(long)]
    pub taskset: String,
    /// Publish the scores.
    #[arg(long)]
    pub public: bool,
    /// Agree to the statement printed on submit (required).
    #[arg(long)]
    pub i_agree: bool,
    /// After submitting, wait for the result like `crucible status --wait`.
    #[arg(long)]
    pub wait: bool,
}

#[derive(clap::Args)]
pub struct AgentArgs {
    #[command(flatten)]
    pub common: Common,
    /// Agent package directory (agent.json + Dockerfile at its root); zipped locally.
    #[arg(
        long,
        required_unless_present = "agent_zip",
        conflicts_with = "agent_zip"
    )]
    pub agent_dir: Option<PathBuf>,
    /// Agent package as a zip.
    #[arg(long)]
    pub agent_zip: Option<PathBuf>,
    /// Run only the first N stages (default: all).
    #[arg(long)]
    pub stages: Option<u32>,
    #[arg(long)]
    pub model: String,
    /// OpenAI-compatible base URL (https).
    #[arg(long)]
    pub endpoint: String,
    #[arg(long, default_value_t = 1)]
    pub replicas: u32,
    #[arg(long)]
    pub max_requests: Option<u64>,
    #[arg(long)]
    pub max_tokens: Option<u64>,
    #[arg(long)]
    pub max_cost_usd: Option<f64>,
    /// Environment variable holding the model API key.
    #[arg(long)]
    pub api_key_env: String,
    /// Environment variable holding the download password (≥ 12 characters).
    #[arg(long)]
    pub download_password_env: String,
}

#[derive(clap::Args)]
pub struct AppArgs {
    #[command(flatten)]
    pub common: Common,
    /// Output zip to score.
    #[arg(long)]
    pub zip: PathBuf,
    /// Stage to score (1-based).
    #[arg(long)]
    pub stage: u32,
}

#[derive(clap::Args)]
pub struct StatusArgs {
    #[command(flatten)]
    pub api: ApiArgs,
    pub eval_id: String,
    /// Poll until the eval is done or failed.
    #[arg(long)]
    pub wait: bool,
    /// Print the raw GET /evals/:id JSON instead of the table.
    #[arg(long)]
    pub json: bool,
}

// ---- payloads (pure, tested) ----------------------------------------------

/// Same field order as the page's `JSON.stringify(cred)`.
#[derive(Serialize)]
struct Credential<'a> {
    api_key: &'a str,
    endpoint: &'a str,
    download_password: &'a str,
    eval_id: &'a str,
}

/// The sealed upload, as the page sends it.
pub fn seal_upload(key: &PublicKey, zip: &[u8]) -> Result<Vec<u8>> {
    check_zip(zip)?;
    let sealed = crucible_crypto::seal(key, zip)?;
    if sealed.len() > MAX_UPLOAD {
        bail!("the sealed zip is larger than the 25 MB limit");
    }
    Ok(sealed)
}

fn check_zip(zip: &[u8]) -> Result<()> {
    if zip.is_empty() {
        bail!("the zip is empty");
    }
    if !zip.starts_with(b"PK\x03\x04") {
        bail!("not a zip file");
    }
    if zip.len() > MAX_PLAIN {
        bail!("the zip is larger than the 25 MB limit");
    }
    Ok(())
}

/// `cred_envelope`: base64 of the sealed credential JSON.
pub fn seal_credential(
    key: &PublicKey,
    api_key: &str,
    endpoint: &str,
    download_password: &str,
    eval_id: &str,
) -> Result<String> {
    let plain = serde_json::to_vec(&Credential {
        api_key: api_key.trim(),
        endpoint: endpoint.trim(),
        download_password,
        eval_id,
    })?;
    Ok(base64_encode(&crucible_crypto::seal(key, &plain)?))
}

/// Lower-case UUID v4 from 16 random bytes.
pub fn uuid_v4(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

fn new_eval_id() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| anyhow!("no randomness: {e}"))?;
    Ok(uuid_v4(b))
}

/// Zip an agent directory the way `crucible fetch` expects it: files at
/// the root, symlinks dropped, `.git` left out.
pub fn zip_agent_dir(dir: &Path) -> Result<Vec<u8>> {
    crate::agentpkg::validate(dir)?;
    let mut entries = Vec::new();
    let mut stats = zipdir::ZipStats::default();
    let skip = |rel: &str| rel == ".git" || rel.starts_with(".git/");
    zipdir::collect(dir, "", &skip, &mut entries, &mut stats)?;
    if stats.bytes as usize > MAX_PLAIN {
        bail!("the agent directory holds more than 25 MB");
    }
    if stats.dropped_links > 0 {
        eprintln!("note: {} symlink(s) left out", stats.dropped_links);
    }
    Ok(zipdir::write_zip(Cursor::new(Vec::new()), &entries, &[])?.into_inner())
}

/// The platform key: `GET /pubkey` when it names a different (valid) key
/// than the bundled one, as the page does; the bundled key otherwise.
fn pick_key(remote: Option<(String, String)>) -> Result<PublicKey> {
    let f: Value = serde_json::from_str(BUNDLED_KEYS)?;
    let current = f["current"].as_str().unwrap_or_default();
    let bundled = f["keys"]
        .as_array()
        .and_then(|ks| ks.iter().find(|k| k["key_id"] == current))
        .and_then(|k| k["public_key"].as_str())
        .ok_or_else(|| anyhow!("bundled keys.json has no current key"))?;
    let (key_id, public) = match remote {
        Some((id, pk)) if id != current => (id, pk),
        _ => (current.to_owned(), bundled.to_owned()),
    };
    let key: PublicKey = public.parse()?;
    if key.key_id() != key_id {
        bail!("the platform public key does not match its key_id");
    }
    Ok(key)
}

// ---- HTTP -------------------------------------------------------------------

struct Api {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Api {
    fn new(a: &ApiArgs) -> Result<Api> {
        let token = std::env::var(TOKEN_ENV).map_err(|_| {
            anyhow!(
                "{TOKEN_ENV} is not set (generate a token on the website: 我的评测 → 命令行令牌)"
            )
        })?;
        if token.trim().is_empty() {
            bail!("{TOKEN_ENV} is empty");
        }
        Ok(Api {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .user_agent(concat!("crucible/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base: a.api.trim().trim_end_matches('/').to_owned(),
            token: token.trim().to_owned(),
        })
    }

    /// One request, retried on network errors and 5xx (4 attempts).
    async fn call(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<(StatusCode, Vec<u8>, u32)> {
        let url = format!("{}{path}", self.base);
        let mut last = None;
        for attempt in 1..=4u32 {
            let mut req = self
                .client
                .request(method.clone(), &url)
                .bearer_auth(&self.token);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            if let Some(b) = &body {
                req = req.body(b.clone());
            }
            match req.send().await {
                Ok(r) if !r.status().is_server_error() => {
                    let status = r.status();
                    return Ok((status, r.bytes().await?.to_vec(), attempt));
                }
                Ok(r) => last = Some(anyhow!("{method} {path}: HTTP {}", r.status())),
                Err(e) => last = Some(anyhow!("{method} {path}: {}", e.without_url())),
            }
            if attempt < 4 {
                tokio::time::sleep(Duration::from_secs(2 * attempt as u64)).await;
            }
        }
        Err(last.expect("at least one attempt"))
    }

    async fn ok_json(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Result<Value> {
        let (status, bytes, _) = self.call(method.clone(), path, headers, body).await?;
        check(&method, path, status, &bytes)?;
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}

fn check(method: &Method, path: &str, status: StatusCode, bytes: &[u8]) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    let v: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let code = v["error"]["code"].as_str().unwrap_or("");
    let msg = v["error"]["message"].as_str().unwrap_or("");
    bail!("{method} {path}: HTTP {status} {code}: {msg}")
}

// ---- commands ---------------------------------------------------------------

fn env_secret(name: &str) -> Result<String> {
    let v = std::env::var(name).map_err(|_| anyhow!("environment variable {name} is not set"))?;
    if v.trim().is_empty() {
        bail!("environment variable {name} is empty");
    }
    Ok(v)
}

fn read_zip(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

pub async fn submit(cmd: SubmitCmd) -> Result<()> {
    let common = match &cmd {
        SubmitCmd::Agent(a) => &a.common,
        SubmitCmd::App(a) => &a.common,
    };
    eprintln!("声明：{CONSENT_TEXT}");
    if !common.i_agree {
        bail!("pass --i-agree to accept the statement above");
    }
    eprintln!("已通过 --i-agree 同意。");
    let eval_id = new_eval_id()?;

    // Validate and read everything local before touching the network.
    let mut secrets: Option<(String, String)> = None;
    let (mode, zip, mut body) = match &cmd {
        SubmitCmd::App(a) => {
            if a.stage == 0 {
                bail!("--stage is 1-based");
            }
            ("app", read_zip(&a.zip)?, json!({"stages": a.stage}))
        }
        SubmitCmd::Agent(a) => {
            if a.model.trim().is_empty() {
                bail!("--model is empty");
            }
            if !(1..=MAX_REPLICAS).contains(&a.replicas) {
                bail!("--replicas must be 1..={MAX_REPLICAS}");
            }
            if a.stages == Some(0) {
                bail!("--stages must be at least 1");
            }
            validate_endpoint(&a.endpoint, false).map_err(|e| anyhow!("--endpoint: {e}"))?;
            let api_key = env_secret(&a.api_key_env)?;
            let password = env_secret(&a.download_password_env)?;
            if password.chars().count() < MIN_PASSWORD {
                bail!("the download password must have at least {MIN_PASSWORD} characters");
            }
            secrets = Some((api_key, password));
            let zip = match (&a.agent_dir, &a.agent_zip) {
                (Some(d), _) => zip_agent_dir(d)?,
                (None, Some(z)) => read_zip(z)?,
                (None, None) => bail!("--agent-dir or --agent-zip is required"),
            };
            let mut b = json!({"model": a.model.trim(), "replicas": a.replicas});
            if let Some(n) = a.stages {
                b["stages"] = json!(n);
            }
            let mut budget = serde_json::Map::new();
            if let Some(v) = a.max_requests {
                budget.insert("max_requests".into(), json!(v));
            }
            if let Some(v) = a.max_tokens {
                budget.insert("max_tokens".into(), json!(v));
            }
            if let Some(v) = a.max_cost_usd {
                budget.insert("max_cost_usd".into(), json!(v));
            }
            if !budget.is_empty() {
                b["budget"] = Value::Object(budget);
            }
            ("agent", zip, b)
        }
    };

    check_zip(&zip)?;
    let api = Api::new(&common.api)?;
    let remote = api
        .ok_json(Method::GET, "/pubkey", &[], None)
        .await
        .ok()
        .and_then(|v| {
            Some((
                v["key_id"].as_str()?.to_owned(),
                v["public_key"].as_str()?.to_owned(),
            ))
        });
    let key = pick_key(remote)?;

    let sealed = seal_upload(&key, &zip)?;
    drop(zip);
    eprintln!("uploading {} sealed bytes…", sealed.len());
    let up = api
        .ok_json(
            Method::POST,
            "/uploads",
            &[
                ("content-type", "application/octet-stream"),
                ("x-upload-kind", mode),
            ],
            Some(sealed),
        )
        .await?;
    let hash = up["hash"]
        .as_str()
        .ok_or_else(|| anyhow!("POST /uploads: no hash in the answer"))?
        .to_owned();

    if let (SubmitCmd::Agent(a), Some((api_key, password))) = (&cmd, &secrets) {
        body["cred_envelope"] = json!(seal_credential(
            &key,
            api_key,
            &a.endpoint,
            password,
            &eval_id
        )?);
    }
    let o = body.as_object_mut().expect("object");
    o.insert("mode".into(), json!(mode));
    o.insert("eval_id".into(), json!(eval_id));
    o.insert("upload_hash".into(), json!(hash));
    o.insert("taskset".into(), json!(common.taskset));
    o.insert("score_public".into(), json!(common.public));
    o.insert("consent".into(), json!(true));

    let path = "/evals";
    let (status, bytes, attempt) = api
        .call(
            Method::POST,
            path,
            &[("content-type", "application/json")],
            Some(serde_json::to_vec(&body)?),
        )
        .await?;
    // A retried POST whose first attempt landed answers 409; it is ours if
    // the eval exists.
    let landed_earlier = status == StatusCode::CONFLICT
        && attempt > 1
        && api
            .ok_json(Method::GET, &format!("/evals/{eval_id}"), &[], None)
            .await
            .is_ok();
    if !landed_earlier {
        check(&Method::POST, path, status, &bytes)?;
    }
    eprintln!(
        "submitted: {}/evals/{eval_id}",
        common.api.api.trim_end_matches('/')
    );
    println!("{eval_id}");
    if common.wait {
        status_cmd(StatusArgs {
            api: ApiArgs {
                api: common.api.api.clone(),
            },
            eval_id,
            wait: true,
            json: false,
        })
        .await?;
    }
    Ok(())
}

pub async fn status_cmd(a: StatusArgs) -> Result<()> {
    let api = Api::new(&a.api)?;
    let path = format!("/evals/{}", a.eval_id.trim());
    let mut last = String::new();
    let detail = loop {
        let d = api.ok_json(Method::GET, &path, &[], None).await?;
        let status = d["status"].as_str().unwrap_or("").to_owned();
        let terminal = status == "done" || status == "failed";
        if !a.wait || terminal {
            break d;
        }
        if status != last {
            eprintln!("status: {status}");
            last = status;
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
    } else {
        print!("{}", render(&detail));
    }
    if detail["status"] == "failed" {
        bail!("eval failed");
    }
    Ok(())
}

// ---- output -----------------------------------------------------------------

fn fmt_secs(s: f64) -> String {
    let s = s.round() as u64;
    match (s / 3600, s % 3600 / 60, s % 60) {
        (0, 0, sec) => format!("{sec}s"),
        (0, m, sec) => format!("{m}m{sec:02}s"),
        (h, m, sec) => format!("{h}h{m:02}m{sec:02}s"),
    }
}

/// Per replica and stage: score, wall time, tokens, equivalent cost; then
/// the total score.
pub fn render(d: &Value) -> String {
    let mut out = format!(
        "eval   {}\nstatus {}\n",
        d["eval_id"].as_str().unwrap_or("?"),
        d["status"].as_str().unwrap_or("?")
    );
    if let Some(u) = d["run_url"].as_str() {
        out += &format!("run    {u}\n");
    }
    let empty = vec![];
    let replicas = d["manifest"]["replicas"].as_array().unwrap_or(&empty);
    // Typed, the old score format reads as the new one; the display comes
    // from the manifest's own snapshot (old manifests: test counts, %).
    let display =
        serde_json::from_value::<crucible_core::taskset::Scoring>(d["manifest"]["scoring"].clone())
            .ok()
            .map(|s| s.display)
            .unwrap_or_else(crucible_core::taskset::legacy_display);
    let (stage_fmt, total_fmt) = (
        display.stage.clone().unwrap_or_default(),
        display.total.clone().unwrap_or_default(),
    );
    if !replicas.is_empty() {
        out += &format!(
            "\n{:<4} {:<16} {:>12} {:>10} {:>12} {:>12} {:>10} {:>10}\n",
            "rep", "stage", "score", "time", "input tok", "cached tok", "output tok", "cost"
        );
    }
    let (mut tokens, mut cost, mut cost_known) = (0u64, 0f64, true);
    for r in replicas {
        let n = r["replica"].as_u64().unwrap_or(0);
        for s in r["stages"].as_array().unwrap_or(&empty) {
            let sc = serde_json::from_value::<crucible_core::StageScore>(s["score"].clone()).ok();
            let score = match sc {
                None => "—".to_owned(),
                Some(sc) => match sc.value() {
                    Some((v, m)) => stage_fmt.fmt(v, m),
                    None => "error".to_owned(),
                },
            };
            let time = s["wall_s"].as_f64().map(fmt_secs).unwrap_or("—".into());
            let u = &s["usage"];
            let (pi, ca, co) = (
                u["prompt_tokens"].as_u64().unwrap_or(0),
                u["cached_tokens"].as_u64().unwrap_or(0),
                u["completion_tokens"].as_u64().unwrap_or(0),
            );
            tokens += pi + co;
            let c = match s["cost_usd"].as_f64() {
                Some(c) => {
                    cost += c;
                    format!("${c:.2}")
                }
                None => {
                    cost_known &= pi + co == 0;
                    "—".into()
                }
            };
            out += &format!(
                "{:<4} {:<16} {:>12} {:>10} {:>12} {:>12} {:>10} {:>10}\n",
                n,
                s["stage"].as_str().unwrap_or("?"),
                score,
                time,
                pi,
                ca,
                co,
                c
            );
        }
    }
    if !replicas.is_empty() {
        out += &format!(
            "\ntokens {tokens}   cost {}\n",
            if cost_known {
                format!("${cost:.2}")
            } else {
                format!("≥ ${cost:.2} (some prices unknown)")
            }
        );
    }
    match d["total_score"].as_f64() {
        Some(t) => out += &format!("total  {}\n", total_fmt.fmt(t, None)),
        None => out += "total  —\n",
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_crypto::PrivateKey;

    #[test]
    fn upload_and_credential_open_like_the_page() {
        let sk = PrivateKey::generate();
        let key = sk.public();
        let keys = std::slice::from_ref(&sk);

        // Upload: a directory zipped locally, sealed; `crucible open` gets the zip back.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.json"),
            br#"{"schema":1,"name":"t","version":"1","streaming":false,"app_start_cmd":["npm","start"]}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("Dockerfile"), b"FROM scratch\n").unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), b"x").unwrap();
        let zip = match zip_agent_dir(dir.path()) {
            Ok(z) => z,
            Err(e) => panic!("{e:#}"),
        };
        let sealed = seal_upload(&key, &zip).unwrap();
        // Envelope header byte-identical to web/src/crypto.ts headerLine().
        let head = format!(
            "{{\"crucible_envelope\":1,\"alg\":\"age-x25519\",\"key_id\":\"{}\"}}\n",
            key.key_id()
        );
        assert!(sealed.starts_with(head.as_bytes()));
        assert!(sealed[head.len()..].starts_with(b"age-encryption.org/v1\n"));
        let plain = crucible_crypto::open(keys, &sealed).unwrap();
        assert_eq!(plain, zip);
        let mut names: Vec<String> = zip::ZipArchive::new(Cursor::new(plain))
            .unwrap()
            .file_names()
            .map(str::to_owned)
            .collect();
        names.sort();
        assert_eq!(names, ["Dockerfile", "agent.json"]);
        assert!(seal_upload(&key, b"not a zip").is_err());

        // Credential: base64(seal(JSON)) with the eval id inside, opened
        // the way Actions does (`crucible cred open`).
        let eid = uuid_v4([0xab; 16]);
        assert_eq!(eid, "abababab-abab-4bab-abab-abababababab");
        let env = seal_credential(
            &key,
            " sk-SECRET ",
            "https://api.example.com/v1",
            "pw-0123456789",
            &eid,
        )
        .unwrap();
        assert!(!env.contains("SECRET"));
        let raw = crate::cred::base64_decode(env.as_bytes()).unwrap();
        assert!(raw.starts_with(head.as_bytes()));
        assert_eq!(
            crucible_crypto::open(keys, &raw).unwrap(),
            format!(
                r#"{{"api_key":"sk-SECRET","endpoint":"https://api.example.com/v1","download_password":"pw-0123456789","eval_id":"{eid}"}}"#
            )
            .as_bytes()
        );
        let line = crate::cred::open_credential(env.as_bytes(), keys, None, Some(&eid)).unwrap();
        assert!(line.contains("sk-SECRET") && !line.contains("pw-"));
        assert_eq!(
            crate::cred::download_password(env.as_bytes(), keys, &eid)
                .unwrap()
                .as_deref(),
            Some("pw-0123456789")
        );
        assert!(crate::cred::open_credential(env.as_bytes(), keys, None, Some("other")).is_err());
    }

    #[test]
    fn platform_key() {
        let bundled = pick_key(None).unwrap();
        assert_eq!(
            pick_key(Some((bundled.key_id(), "junk".into())))
                .unwrap()
                .key_id(),
            bundled.key_id()
        );
        let other = PrivateKey::generate().public();
        let rotated = pick_key(Some((other.key_id(), other.to_string()))).unwrap();
        assert_eq!(rotated.key_id(), other.key_id());
        assert!(pick_key(Some(("0000000000000000".into(), other.to_string()))).is_err());
    }

    #[test]
    fn renders_scores() {
        let d = json!({
            "eval_id": "e", "status": "done", "total_score": 0.775,
            "manifest": {"replicas": [{"replica": 1, "stages": [
                {"stage": "stage-1", "score": {"status": "failed", "passed": 25, "total": 30},
                 "wall_s": 723.0, "usage": {"prompt_tokens": 1000, "cached_tokens": 600, "completion_tokens": 50},
                 "cost_usd": 0.12}
            ]}]}
        });
        let s = render(&d);
        assert!(s.contains("25/30") && s.contains("12m03s") && s.contains("$0.12"));
        assert!(s.contains("total  77.5%") && s.contains("tokens 1050"));

        // Result v2 with a display snapshot (astro-practice).
        let d = json!({
            "eval_id": "e", "status": "done", "total_score": 4458.556,
            "manifest": {"schema": 2, "eval_id": "e", "created_at": "t", "taskset": "astro-practice",
              "agent": {"name": "a", "version": "1"}, "model": "m",
              "scoring": {"aggregate": {"stages": "sum"},
                "display": {"stage": {"name": "观测得分", "decimals": 2}, "total": {"name": "四卡总分", "decimals": 2}},
                "plugins": [{"kind": "scorer", "name": "astro-survey", "version": "2"}]},
              "replicas": [{"replica": 1, "stages": [
                {"stage": "l1", "score": {"status": "scored", "score": 4458.556},
                 "usage": {"requests": 0, "prompt_tokens": 0, "cached_tokens": 0, "completion_tokens": 0, "reasoning_tokens": 0}}
            ]}]}
        });
        let s = render(&d);
        assert!(s.contains("4458.56") && s.contains("total  4458.56"), "{s}");
    }
}
