//! `crucible plan`: validate the dispatch inputs of `eval.yml` and turn them
//! into job outputs. Inputs arrive as environment variables (never spliced
//! into a shell line); every value is checked against a strict pattern
//! before it is echoed back as an output.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use crucible_core::netpolicy::validate_endpoint;
use crucible_core::taskset::MAX_TOTAL_TIME_S;
use serde::{Deserialize, Serialize};

use crate::agentpkg::Source;
use crate::cred::CredSource;

pub const MAX_REPLICAS: u32 = 20;
/// The run step's timeout must leave room in the 360-minute job for
/// checkout, the agent build and sealing the outputs.
pub const MAX_RUN_STEP_MIN: u64 = 330;

#[derive(Debug, Clone, Default, clap::Args)]
pub struct PlanInputs {
    #[arg(long, env = "IN_AGENT_SOURCE", default_value = "")]
    pub agent_source: String,
    #[arg(long, env = "IN_TASKSET", default_value = "")]
    pub taskset: String,
    #[arg(long, env = "IN_MODEL", default_value = "")]
    pub model: String,
    #[arg(long, env = "IN_ENDPOINT", default_value = "")]
    pub endpoint: String,
    #[arg(long, env = "IN_REPLICAS", default_value = "1")]
    pub replicas: String,
    #[arg(long, env = "IN_CRED_SOURCE", default_value = "")]
    pub cred_source: String,
    #[arg(long, env = "IN_BUDGET", default_value = "")]
    pub budget: String,
    #[arg(long, env = "IN_STAGES", default_value = "")]
    pub stages: String,
    #[arg(long, env = "IN_EVAL_ID", default_value = "")]
    pub eval_id: String,
    /// `true` publishes the manifest in clear on the data branch.
    #[arg(long, env = "IN_SCORE_PUBLIC", default_value = "false")]
    pub score_public: String,
    /// `<github_id>:<login>` of the submitter (optional).
    #[arg(long, env = "IN_OWNER", default_value = "")]
    pub owner: String,
    /// JSON {budget?, stages?, results_url?} (workflow_dispatch allows only
    /// 10 inputs, so the optional knobs share one).
    #[arg(long, env = "IN_OPTIONS", default_value = "")]
    pub options: String,
    /// The Worker when `results_url` does not name one (repository
    /// variable `CRUCIBLE_WORKER_URL`).
    #[arg(long, env = "IN_WORKER_URL", default_value = "")]
    pub worker_url: String,
    /// Used for the default eval id.
    #[arg(long, env = "GITHUB_RUN_ID", default_value = "")]
    pub run_id: String,
    #[arg(long, env = "GITHUB_RUN_ATTEMPT", default_value = "1")]
    pub run_attempt: String,
}

/// Inputs of `score.yml` (upload an output, score one stage).
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ScorePlanInputs {
    #[arg(long, env = "IN_EVAL_ID", default_value = "")]
    pub eval_id: String,
    /// `blob:<sha256>` of the sealed upload.
    #[arg(long, env = "IN_ARTIFACT_SOURCE", default_value = "")]
    pub artifact_source: String,
    #[arg(long, env = "IN_TASKSET", default_value = "")]
    pub taskset: String,
    /// 1-based stage number.
    #[arg(long, env = "IN_STAGE", default_value = "")]
    pub stage: String,
    /// `none` or `workers-kv` (only used to delete the credential).
    #[arg(long, env = "IN_CRED_SOURCE", default_value = "none")]
    pub cred_source: String,
    #[arg(long, env = "IN_SCORE_PUBLIC", default_value = "false")]
    pub score_public: String,
    #[arg(long, env = "IN_OWNER", default_value = "")]
    pub owner: String,
    #[arg(long, env = "IN_RESULTS_URL", default_value = "")]
    pub results_url: String,
    #[arg(long, env = "IN_WORKER_URL", default_value = "")]
    pub worker_url: String,
    #[arg(long, env = "GITHUB_RUN_ID", default_value = "")]
    pub run_id: String,
    #[arg(long, env = "GITHUB_RUN_ATTEMPT", default_value = "1")]
    pub run_attempt: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    #[serde(default)]
    budget: Option<serde_json::Value>,
    #[serde(default)]
    stages: Option<u32>,
    #[serde(default)]
    results_url: Option<String>,
    /// Builtin agents built from another repo (upstream.json): a branch or
    /// tag of that repo to build instead of upstream.json's ref.
    #[serde(default)]
    agent_ref: Option<String>,
}

pub fn owner_ok(o: &str) -> bool {
    match o.split_once(':') {
        Some((id, login)) => {
            !id.is_empty()
                && id.len() <= 12
                && id.bytes().all(|b| b.is_ascii_digit())
                && !login.is_empty()
                && login.len() <= 39
                && login
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }
        None => false,
    }
}

/// Optional caps for the whole run (all stages together).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// User price (USD per 1M tokens), wins over config/pricing.json.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<serde_json::Value>,
}

impl Budget {
    pub fn parse(raw: &str) -> Result<Budget> {
        if raw.trim().is_empty() {
            return Ok(Budget::default());
        }
        let b: Budget = serde_json::from_str(raw).map_err(|_| {
            anyhow!("budget must be JSON {{max_requests?, max_tokens?, max_cost_usd?, price?}}")
        })?;
        if b.max_requests == Some(0) || b.max_tokens == Some(0) {
            bail!("budget caps must be > 0");
        }
        if b.max_cost_usd.is_some_and(|c| !c.is_finite() || c <= 0.0) {
            bail!("budget.max_cost_usd must be > 0");
        }
        if let Some(p) = &b.price {
            crucible_metering::parse_price(p)
                .map_err(|e| anyhow!("budget.price: {e}"))?
                .ok_or_else(|| anyhow!("budget.price needs input and output"))?;
        }
        Ok(b)
    }
}

pub fn model_ok(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 80
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

pub fn eval_id_ok(id: &str) -> bool {
    id.len() >= 8 && crucible_core::is_slug(id, 64)
}

fn check_url(what: &str, url: &str) -> Result<()> {
    if url.is_empty() {
        return Ok(());
    }
    validate_endpoint(url, false).map_err(|e| anyhow!("{what}: {e}"))?;
    if url.bytes().any(|b| !b.is_ascii_graphic()) {
        bail!("{what} has unsupported characters");
    }
    Ok(())
}

/// The Worker: the origin of `results_url` when given, else `fallback`.
fn worker_url(results_url: &str, fallback: &str) -> Result<String> {
    if !results_url.is_empty() {
        return crate::worker::origin(results_url);
    }
    let f = fallback.trim().trim_end_matches('/');
    if f.is_empty() {
        return Ok(String::new());
    }
    check_url("worker_url", f)?;
    crate::worker::origin(f)
}

fn eval_id_or_default(id: &str, run_id: &str, run_attempt: &str) -> Result<String> {
    match id.trim() {
        "" => {
            let run: u64 = run_id
                .trim()
                .parse()
                .map_err(|_| anyhow!("eval_id is empty and GITHUB_RUN_ID is not set"))?;
            let attempt: u32 = run_attempt.trim().parse().unwrap_or(1);
            Ok(format!("dev-{run}-{attempt}"))
        }
        id if eval_id_ok(id) => Ok(id.to_owned()),
        _ => bail!("eval_id must match [a-z0-9][a-z0-9-]{{7,63}}"),
    }
}

/// A user-uploaded taskset (`u-...`) is not in the repository: fetch its
/// taskset.json from the Worker into `tasksets/<id>/taskset.json`, which
/// the Worker only hands out if the submitter (`owner`) may use it (the
/// uploader, or anyone once an admin made it public). Built-in names: no-op.
pub async fn fetch_user_taskset(
    root: &Path,
    taskset: &str,
    owner: &str,
    results_url: &str,
    worker_fallback: &str,
) -> Result<()> {
    let id = taskset.trim();
    if !id.starts_with("u-") {
        return Ok(());
    }
    if !crucible_core::taskset::is_user_taskset_id(id) {
        bail!("taskset u-... must be u-<16 hex>");
    }
    let owner = owner.trim();
    let gid: u64 = owner
        .split_once(':')
        .filter(|_| owner_ok(owner))
        .and_then(|(g, _)| g.parse().ok())
        .ok_or_else(|| anyhow!("a user taskset needs owner <github_id>:<login>"))?;
    check_url("results_url", results_url.trim())?;
    let worker = worker_url(results_url.trim(), worker_fallback)?;
    if worker.is_empty() {
        bail!("a user taskset needs a Worker (results_url or CRUCIBLE_WORKER_URL)");
    }
    let raw = crate::worker::Worker::from_env(&worker)?
        .get_user_taskset(id, gid)
        .await?;
    let ts: crucible_core::TaskSet =
        serde_json::from_slice(&raw).map_err(|e| anyhow!("taskset {id} from the Worker: {e}"))?;
    if ts.name != id {
        bail!("taskset {id} from the Worker is named {:?}", ts.name);
    }
    ts.validate(MAX_TOTAL_TIME_S)?;
    let dir = root.join("tasksets").join(id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("taskset.json"),
        serde_json::to_string_pretty(&ts)? + "\n",
    )?;
    eprintln!("user taskset {id}: {} stages", ts.stages.len());
    Ok(())
}

/// `results_url` inside eval.yml's `options` (empty if absent or invalid;
/// `plan` reports invalid options itself).
pub fn options_results_url(options: &str) -> String {
    serde_json::from_str::<Options>(options)
        .ok()
        .and_then(|o| o.results_url)
        .unwrap_or_default()
}

fn load_taskset(root: &Path, name: &str) -> Result<(String, crucible_core::TaskSet)> {
    if !crucible_core::is_slug(name, 64) {
        bail!("taskset must match [a-z0-9][a-z0-9-]{{0,63}}");
    }
    let ts_path = format!("tasksets/{name}/taskset.json");
    if !root.join(&ts_path).is_file() {
        bail!("unknown taskset {name:?}");
    }
    let ts = crate::taskset_cmd::load(&root.join(&ts_path))?;
    ts.validate(MAX_TOTAL_TIME_S)?;
    Ok((ts_path, ts))
}

fn score_public(s: &str) -> Result<bool> {
    match s.trim() {
        "" | "false" => Ok(false),
        "true" => Ok(true),
        _ => bail!("score_public must be true or false"),
    }
}

/// Validate the `score.yml` inputs.
pub fn plan_score(inp: &ScorePlanInputs, root: &Path) -> Result<BTreeMap<&'static str, String>> {
    let hash = inp
        .artifact_source
        .trim()
        .strip_prefix("blob:")
        .filter(|h| crucible_core::blob::is_sha256_hex(h))
        .ok_or_else(|| anyhow!("artifact_source must be blob:<sha256>"))?;
    let (ts_path, ts) = load_taskset(root, inp.taskset.trim())?;
    let stage: usize = inp
        .stage
        .trim()
        .parse()
        .ok()
        .filter(|n| (1..=ts.stages.len()).contains(n))
        .ok_or_else(|| anyhow!("stage must be 1..={}", ts.stages.len()))?;
    let cred = match inp.cred_source.trim() {
        "" | "none" => "none",
        "workers-kv" => "workers-kv",
        _ => bail!("cred_source must be none or workers-kv"),
    };
    let results_url = inp.results_url.trim();
    check_url("results_url", results_url)?;
    let worker = worker_url(results_url, &inp.worker_url)?;
    if cred == "workers-kv" && worker.is_empty() {
        bail!("cred_source workers-kv needs a Worker (results_url or CRUCIBLE_WORKER_URL)");
    }
    let owner = inp.owner.trim();
    if !owner.is_empty() && !owner_ok(owner) {
        bail!("owner must be <github_id>:<login>");
    }
    let mut out = BTreeMap::new();
    out.insert(
        "eval_id",
        eval_id_or_default(&inp.eval_id, &inp.run_id, &inp.run_attempt)?,
    );
    out.insert("artifact_hash", hash.to_owned());
    out.insert("taskset", ts.name.clone());
    out.insert("taskset_path", ts_path);
    out.insert("stage", stage.to_string());
    out.insert("stage_id", ts.stages[stage - 1].id.clone());
    out.insert("cred_source", cred.to_owned());
    out.insert("score_public", score_public(&inp.score_public)?.to_string());
    out.insert("owner", owner.to_owned());
    out.insert("results_url", results_url.to_owned());
    out.insert("worker_url", worker);
    Ok(out)
}

pub fn plan(inp: &PlanInputs, root: &Path) -> Result<BTreeMap<&'static str, String>> {
    let source = Source::parse(inp.agent_source.trim())?;
    if let Source::Builtin(name) = &source
        && !root.join("agents").join(name).join("agent.json").is_file()
    {
        bail!("unknown builtin agent {name:?}");
    }
    let model = inp.model.trim();
    if !model_ok(model) {
        bail!("model must match [A-Za-z0-9._:/-]{{1,80}}");
    }
    let endpoint = inp.endpoint.trim();
    if !endpoint.is_empty() {
        validate_endpoint(endpoint, false)?;
        if endpoint.bytes().any(|b| !b.is_ascii_graphic()) {
            bail!("endpoint has unsupported characters");
        }
    }
    let replicas: u32 = inp
        .replicas
        .trim()
        .parse()
        .map_err(|_| anyhow!("replicas must be an integer"))?;
    if !(1..=MAX_REPLICAS).contains(&replicas) {
        bail!("replicas must be 1..={MAX_REPLICAS}");
    }
    let cred = inp.cred_source.trim();
    let cred_source = CredSource::parse(cred)?;
    let opts: Options = if inp.options.trim().is_empty() {
        Options::default()
    } else {
        serde_json::from_str(&inp.options).map_err(|_| {
            anyhow!("options must be JSON {{budget?, stages?, results_url?, agent_ref?}}")
        })?
    };
    let budget_raw = match (&opts.budget, inp.budget.trim()) {
        (Some(_), b) if !b.is_empty() => bail!("budget given twice"),
        (Some(v), _) => v.to_string(),
        (None, b) => b.to_owned(),
    };
    let budget = Budget::parse(&budget_raw)?;
    let stages_raw = match (opts.stages, inp.stages.trim()) {
        (Some(_), s) if !s.is_empty() => bail!("stages given twice"),
        (Some(n), _) => n.to_string(),
        (None, s) => s.to_owned(),
    };
    let agent_ref = opts.agent_ref.unwrap_or_default();
    if !agent_ref.is_empty()
        && !(matches!(source, Source::Builtin(_)) && crate::agentpkg::ref_ok(&agent_ref))
    {
        bail!("agent_ref must be a branch or tag name, for a builtin agent only");
    }
    let results_url = opts.results_url.unwrap_or_default();
    check_url("results_url", &results_url)?;
    let worker = worker_url(&results_url, &inp.worker_url)?;
    if cred_source == CredSource::WorkersKv && worker.is_empty() {
        bail!("cred_source workers-kv needs a Worker (results_url or CRUCIBLE_WORKER_URL)");
    }
    let score_public = score_public(&inp.score_public)?;
    let owner = inp.owner.trim();
    if !owner.is_empty() && !owner_ok(owner) {
        bail!("owner must be <github_id>:<login>");
    }

    let (ts_path, ts) = load_taskset(root, inp.taskset.trim())?;
    let n = match stages_raw.as_str() {
        "" => ts.stages.len(),
        s => s
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=ts.stages.len()).contains(n))
            .ok_or_else(|| anyhow!("stages must be 1..={}", ts.stages.len()))?,
    };
    let stage_s: u64 = ts.stages.iter().take(n).map(|s| s.time_limit_s).sum();
    let run_min = stage_s.div_ceil(60) + 3 * n as u64 + 5;
    if run_min > MAX_RUN_STEP_MIN {
        bail!("{n} stages need a {run_min}-minute run step, more than {MAX_RUN_STEP_MIN}");
    }

    let eval_id = eval_id_or_default(&inp.eval_id, &inp.run_id, &inp.run_attempt)?;

    let mut out = BTreeMap::new();
    out.insert("agent_source", inp.agent_source.trim().to_owned());
    out.insert("agent_kind", source.kind().to_owned());
    out.insert("agent_ref", agent_ref);
    out.insert("model", model.to_owned());
    out.insert("endpoint", endpoint.to_owned());
    out.insert("cred_source", cred.to_owned());
    out.insert(
        "budget",
        if budget == Budget::default() {
            String::new()
        } else {
            serde_json::to_string(&budget)?
        },
    );
    out.insert("taskset", ts.name.clone());
    out.insert("taskset_path", ts_path);
    out.insert("stages", n.to_string());
    out.insert("run_timeout_min", run_min.to_string());
    out.insert("eval_id", eval_id);
    out.insert("score_public", score_public.to_string());
    out.insert("owner", owner.to_owned());
    out.insert("results_url", results_url);
    out.insert("worker_url", worker);
    out.insert(
        "matrix",
        serde_json::to_string(
            &serde_json::json!({ "replica": (1..=replicas).collect::<Vec<_>>() }),
        )?,
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("agents/octos")).unwrap();
        std::fs::write(d.path().join("agents/octos/agent.json"), "{}").unwrap();
        std::fs::create_dir_all(d.path().join("tasksets/demo")).unwrap();
        let blob = format!(r#"{{"sha256":"{}","key_id":"k1"}}"#, "a".repeat(64));
        std::fs::write(
            d.path().join("tasksets/demo/taskset.json"),
            format!(
                r#"{{"schema":1,"name":"demo","scorer":{{"name":"playwright"}},"total_time_limit_s":15600,
                "stages":[{{"id":"stage-1","inputs_blob":{blob},"tests_blob":{blob},"output":"web-app","time_limit_s":4800}},
                          {{"id":"stage-2","inputs_blob":{blob},"tests_blob":{blob},"output":"web-app","time_limit_s":4800}},
                          {{"id":"stage-3","inputs_blob":{blob},"tests_blob":{blob},"output":"web-app","time_limit_s":6000}}]}}"#
            ),
        )
        .unwrap();
        d
    }

    fn good() -> PlanInputs {
        PlanInputs {
            agent_source: "builtin:octos".into(),
            taskset: "demo".into(),
            model: "glm-5.3-flash".into(),
            replicas: "2".into(),
            cred_source: "github-secret".into(),
            run_id: "123".into(),
            run_attempt: "1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn plans() {
        let r = root();
        let out = plan(&good(), r.path()).unwrap();
        assert_eq!(out["matrix"], r#"{"replica":[1,2]}"#);
        assert_eq!(out["stages"], "3");
        assert_eq!(out["run_timeout_min"], (260 + 9 + 5).to_string());
        assert_eq!(out["eval_id"], "dev-123-1");
        assert_eq!(out["budget"], "");
        let mut i = good();
        i.stages = "1".into();
        i.budget = r#"{"max_cost_usd": 5}"#.into();
        i.eval_id = "my-eval-0001".into();
        let out = plan(&i, r.path()).unwrap();
        assert_eq!(out["stages"], "1");
        assert_eq!(out["run_timeout_min"], "88");
        assert_eq!(out["budget"], r#"{"max_cost_usd":5.0}"#);
        assert_eq!(out["eval_id"], "my-eval-0001");
        assert_eq!(out["score_public"], "false");
        let mut i = good();
        i.options = r#"{"stages":2,"budget":{"max_requests":100},"results_url":"https://crucible.example.workers.dev/results"}"#.into();
        i.score_public = "true".into();
        i.owner = "123456:octo-cat".into();
        let out = plan(&i, r.path()).unwrap();
        assert_eq!(out["stages"], "2");
        assert_eq!(out["budget"], r#"{"max_requests":100}"#);
        assert_eq!(out["score_public"], "true");
        assert_eq!(out["owner"], "123456:octo-cat");
        assert!(out["results_url"].starts_with("https://"));
        assert_eq!(out["worker_url"], "https://crucible.example.workers.dev");
        assert_eq!(out["agent_ref"], "");
        let mut i = good();
        i.options = r#"{"agent_ref":"loop/x-1"}"#.into();
        assert_eq!(plan(&i, r.path()).unwrap()["agent_ref"], "loop/x-1");
        i.options = r#"{"agent_ref":"--upload-pack=x"}"#.into();
        assert!(plan(&i, r.path()).is_err());
        assert_eq!(plan(&good(), r.path()).unwrap()["worker_url"], "");
        let mut i = good();
        i.cred_source = "workers-kv".into();
        i.worker_url = "https://w.example.dev/".into();
        let out = plan(&i, r.path()).unwrap();
        assert_eq!(
            (out["cred_source"].as_str(), out["worker_url"].as_str()),
            ("workers-kv", "https://w.example.dev")
        );
    }

    #[test]
    fn plans_score() {
        let r = root();
        let good = || ScorePlanInputs {
            artifact_source: format!("blob:{}", "ab".repeat(32)),
            taskset: "demo".into(),
            stage: "2".into(),
            cred_source: "none".into(),
            run_id: "9".into(),
            ..Default::default()
        };
        let out = plan_score(&good(), r.path()).unwrap();
        assert_eq!(out["stage_id"], "stage-2");
        assert_eq!(out["artifact_hash"], "ab".repeat(32));
        assert_eq!(out["eval_id"], "dev-9-1");
        assert_eq!(out["worker_url"], "");
        let mut i = good();
        i.results_url = "https://w.example.dev/internal/results/x".into();
        i.cred_source = "workers-kv".into();
        assert_eq!(
            plan_score(&i, r.path()).unwrap()["worker_url"],
            "https://w.example.dev"
        );
        type Case = (&'static str, Box<dyn Fn(&mut ScorePlanInputs)>);
        let cases: Vec<Case> = vec![
            (
                "source",
                Box::new(|i| i.artifact_source = "url:https://x".into()),
            ),
            ("hash", Box::new(|i| i.artifact_source = "blob:AB".into())),
            ("stage 0", Box::new(|i| i.stage = "0".into())),
            ("stage 4", Box::new(|i| i.stage = "4".into())),
            ("cred", Box::new(|i| i.cred_source = "github-secret".into())),
            ("kv", Box::new(|i| i.cred_source = "workers-kv".into())),
            ("public", Box::new(|i| i.score_public = "yes".into())),
            ("owner", Box::new(|i| i.owner = "x".into())),
        ];
        for (why, f) in cases {
            let mut i = good();
            f(&mut i);
            assert!(plan_score(&i, r.path()).is_err(), "{why}");
        }
    }

    #[test]
    fn rejects() {
        let r = root();
        type Case = (&'static str, Box<dyn Fn(&mut PlanInputs)>);
        let cases: Vec<Case> = vec![
            (
                "agent",
                Box::new(|i| i.agent_source = "builtin:nope".into()),
            ),
            (
                "agent kind",
                Box::new(|i| i.agent_source = "docker:x".into()),
            ),
            ("model", Box::new(|i| i.model = "glm; rm -rf /".into())),
            ("model empty", Box::new(|i| i.model = "".into())),
            (
                "endpoint http",
                Box::new(|i| i.endpoint = "http://api.z.ai/v4".into()),
            ),
            (
                "endpoint private",
                Box::new(|i| i.endpoint = "https://10.0.0.1/v1".into()),
            ),
            ("replicas 0", Box::new(|i| i.replicas = "0".into())),
            ("replicas 21", Box::new(|i| i.replicas = "21".into())),
            ("replicas text", Box::new(|i| i.replicas = "1;id".into())),
            ("cred", Box::new(|i| i.cred_source = "env".into())),
            (
                "kv without a Worker",
                Box::new(|i| i.cred_source = "workers-kv".into()),
            ),
            (
                "worker http",
                Box::new(|i| i.worker_url = "http://w.example.dev".into()),
            ),
            (
                "budget",
                Box::new(|i| i.budget = r#"{"max_requests":0}"#.into()),
            ),
            (
                "budget unknown",
                Box::new(|i| i.budget = r#"{"deadline":1}"#.into()),
            ),
            ("taskset", Box::new(|i| i.taskset = "../demo".into())),
            ("taskset missing", Box::new(|i| i.taskset = "other".into())),
            ("stages 4", Box::new(|i| i.stages = "4".into())),
            ("stages 0", Box::new(|i| i.stages = "0".into())),
            ("eval id", Box::new(|i| i.eval_id = "Short".into())),
            (
                "eval id path",
                Box::new(|i| i.eval_id = "../../etc/passwd".into()),
            ),
        ];
        for (why, f) in cases {
            let mut i = good();
            f(&mut i);
            assert!(plan(&i, r.path()).is_err(), "{why}");
        }
    }
}
