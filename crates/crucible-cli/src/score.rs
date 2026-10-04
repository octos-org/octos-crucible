//! `crucible score`: score each stage checkpoint with the taskset's scorer.
//!
//! Input layout (same as the publish job's): `<results>/<replica>/<stage>/
//! checkpoint.sealed` (plus `logs.sealed` when the stage ran). Per stage the
//! tests blob is fetched and opened into a temporary directory that is
//! deleted as soon as that stage is scored; each checkpoint is opened into a
//! temporary file the same way.
//!
//! Isolation (docs/scorer-contract.md §7): in the workflows the tests run
//! on a machine that holds no secret. `handoff` (on the machine with the
//! platform key) re-seals the selected tests blobs and checkpoints to a
//! fresh one-run key; `score` then runs elsewhere with only that key, read
//! from `TestsFrom::Dir`, and the scorer process never sees it. Each
//! stage's scorer is looked up in the plugin registry (`plugins.json`):
//! `<plugins root>/<impl>/score.sh` (docs/scorer-contract.md), run with
//! `--visibility hidden`, so its
//! result.json holds only the score, fixed-name items and a fixed detail text:
//! nothing of the tests leaves this job.
//!
//! Before the scorer runs, the output is checked against the stage's
//! packager format (an uploaded output may be anything); one that does not
//! match scores 0.
//!
//! Output: `<out>/<replica>/<stage>/score.json`, always result v2 (an old
//! format result is converted), normalised for the manifest:
//! - only items: score / max from them (the taskset's `aggregate.items`);
//! - no score, or out of bounds: `error` (system);
//! - a counted stage (`expected_total`) scored with no test results (build
//!   failed, app never ready): score 0, max = `expected_total`;
//! - a test count that differs from `expected_total`: `error` (flagged,
//!   not summed).

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use crucible_core::plugins::Kind;
use crucible_core::score::ErrorKind;
use crucible_core::taskset::{Aggregate, ModelUse, Stage};
use crucible_core::{ScoreResult, TaskSet};
use crucible_crypto::{PrivateKey, PublicKey};
use crucible_meter::{Credential, Limits, MeterConfig, Upstream};
use crucible_metering::{Price, Pricing};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::executor::Executor;
use crate::keys::Store;
use crate::plan::Budget;
use crate::runners::workdir::{Used, read_usage, remaining, used};
use crate::zipdir::{self, ExtractLimits};

/// Limits for unpacking a tests blob.
pub const TESTS_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 5_000,
    max_bytes: 256 << 20,
};

/// Where the tests of a stage come from.
pub enum TestsFrom<'a> {
    /// The taskset's tests blob in the store.
    Store(&'a Store),
    /// `<dir>/<stage id>.sealed`, written by `handoff`.
    Dir(&'a Path),
}

pub struct ScoreOpts<'a> {
    pub taskset: &'a TaskSet,
    /// Indices into `taskset.stages` to score.
    pub stages: Vec<usize>,
    pub results: &'a Path,
    pub out: &'a Path,
    pub tests: TestsFrom<'a>,
    pub keys: &'a [PrivateKey],
    /// Where the registry's plugin directories are (`scorers/<name>/`).
    pub plugins_root: &'a Path,
    /// Environment variables never passed to the plugins (the key names).
    pub scrub_env: &'a [String],
    /// Score only this replica (one scoring job per replica).
    pub replica: Option<u32>,
    /// The submitter's model for slots that use one.
    pub model: Option<&'a ModelSetup>,
    /// The step's run label: the plugins' containers carry it too.
    pub run_label: Option<&'a str>,
}

/// Normalise a scorer result for the manifest: [`ScoreResult::finish`]
/// (score from items, bounds), then the stage's declared test count.
pub fn normalise(r: ScoreResult, expected_total: Option<u32>, agg: &Aggregate) -> ScoreResult {
    let mut r = r.finish(agg);
    if !r.status.is_scored() {
        return r;
    }
    if let Some(e) = expected_total {
        let e = f64::from(e);
        match r.max {
            None | Some(0.0) => {
                r.score = Some(0.0);
                r.max = Some(e);
            }
            Some(m) if m != e => {
                return ScoreResult {
                    visibility: r.visibility,
                    task_id: r.task_id,
                    submission_id: r.submission_id,
                    ..ScoreResult::error(
                        ErrorKind::System,
                        format!("scorer reported {m} tests, the taskset expects {e}"),
                    )
                };
            }
            _ => {}
        }
    }
    r
}

fn system_error(detail: impl Into<String>) -> ScoreResult {
    ScoreResult {
        visibility: Some("hidden".into()),
        ..ScoreResult::error(ErrorKind::System, detail)
    }
}

/// Scored 0: the agent produced nothing to score.
fn zero(detail: impl Into<String>) -> ScoreResult {
    ScoreResult {
        visibility: Some("hidden".into()),
        ..ScoreResult::scored(0.0, None, detail)
    }
}

/// Numeric replica directories under `results`, sorted.
pub fn replicas(results: &Path) -> Result<Vec<u32>> {
    let mut out: Vec<u32> = std::fs::read_dir(results)
        .with_context(|| format!("reading {}", results.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    out.sort();
    Ok(out)
}

/// A container plugin: its entry script, and the image the scoring job
/// built for it in advance (`crucible-<kind>-<name>:run`), if any.
pub struct ContainerPlugin {
    pub entry: PathBuf,
    pub image: Option<String>,
    /// `CRUCIBLE_RUN_LABEL` of the script (its containers carry it).
    pub run_label: Option<String>,
}

/// The generic shell that runs an uploaded scorer's image
/// (docs/plugins.md §14), relative to the plugins root.
pub const USER_SCORER_SHELL: &str = "scorers/_user/score.sh";

/// Resolve a container plugin of the taskset (registry, or uploaded)
/// under `root`.
pub async fn container_plugin(
    root: &Path,
    ts: &TaskSet,
    kind: Kind,
    reference: &str,
    entry: &str,
) -> Result<ContainerPlugin> {
    let p = ts
        .plugin(kind, reference)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if p.is_builtin() {
        bail!("{} {} is not a container plugin", kind.as_str(), p.name);
    }
    let entry = if p.is_user() {
        root.join(USER_SCORER_SHELL)
    } else {
        root.join(&p.implementation).join(entry)
    };
    if !entry.is_file() {
        bail!(
            "{} {} not found at {}",
            kind.as_str(),
            p.name,
            entry.display()
        );
    }
    // Built by `step score-tests` (else the script builds its own; an
    // uploaded plugin has no script of its own and must be built there).
    let tag = format!("crucible-{}-{}:run", kind.as_str(), p.name);
    let exec = crate::executor::backend()?;
    let image = exec
        .image_exists(&exec.image_ref(&tag))
        .await
        .then(|| exec.image_ref(&tag));
    if p.is_user() && image.is_none() {
        bail!(
            "{} {}: its image {tag} was not built (crucible step score-tests builds it)",
            kind.as_str(),
            p.name
        );
    }
    Ok(ContainerPlugin {
        entry,
        image,
        run_label: None,
    })
}

/// The two slots of the scoring job that may use a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Interactive,
    Scorer,
}

impl Slot {
    fn name(self) -> &'static str {
        match self {
            Slot::Interactive => "interactive",
            Slot::Scorer => "scorer",
        }
    }
}

/// Everything the scoring job needs to give a slot the submitter's model
/// (docs/plugins.md §10). The credential lives only here, in this
/// process: plugins get the meter's address and nothing else.
pub struct ModelSetup {
    pub cred: Credential,
    /// The submitter's model; a taskset's `model.name` wins.
    pub model: String,
    pub pricing: Pricing,
    pub user_price: Option<Price>,
    /// Docker network the plugin containers join; its only reachable
    /// address is `bind:port` (tools/sandbox-net.sh).
    pub network: String,
    pub bind: String,
    pub port: u16,
    /// Per replica: the caps of the whole evaluation minus what the
    /// generation already used (empty: no caps).
    pub budgets: BTreeMap<u32, Budget>,
}

/// The JSON `score-handoff` writes next to the sealed credential.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ModelPlan {
    pub model: String,
    #[serde(default)]
    pub budgets: BTreeMap<u32, Budget>,
    /// The submitter's own price (`budget.price`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<serde_json::Value>,
}

/// A meter for one slot run, with its own usage log.
struct SlotMeter {
    task: tokio::task::JoinHandle<std::result::Result<(), crucible_meter::ConfigError>>,
    log: PathBuf,
}

impl SlotMeter {
    async fn stop(self) -> Used {
        self.task.abort();
        let _ = self.task.await;
        used(&read_usage(&self.log))
    }
}

/// The model a slot of `stage` runs with, if any: `Ok(None)` = run without
/// one; `Err` = the stage requires a model and none was given.
fn slot_model(ts: &TaskSet, slot: Slot, m: Option<&ModelSetup>) -> Result<Option<String>, String> {
    let decl = ts.model.clone().unwrap_or_default();
    let use_ = match slot {
        Slot::Interactive => decl.interactive,
        Slot::Scorer => decl.scorer,
    };
    if use_ == ModelUse::None {
        return Ok(None);
    }
    let name = decl
        .name
        .clone()
        .or_else(|| m.map(|m| m.model.clone()))
        .filter(|n| !n.is_empty());
    match (m, name) {
        (Some(_), Some(n)) => Ok(Some(n)),
        _ if use_ == ModelUse::Required => Err(format!(
            "the {} of this stage needs a model credential",
            slot.name()
        )),
        _ => Ok(None),
    }
}

/// Start the slot's meter and return it with the plugin's model arguments.
async fn start_meter(
    m: &ModelSetup,
    model: &str,
    log: PathBuf,
    limits: Limits,
    network_flag: &str,
) -> Result<(SlotMeter, Vec<String>)> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::File::create(&log)?;
    let cfg = MeterConfig {
        upstream: Upstream::new(m.cred.clone(), false)?,
        model: model.to_owned(),
        pricing: m.pricing.clone(),
        user_price: m.user_price,
        force_usage: true,
        limits,
        log_path: log.clone(),
        insecure_allow_loopback_for_tests: false,
    };
    let listener = TcpListener::bind((m.bind.as_str(), m.port))
        .await
        .with_context(|| format!("meter: binding {}:{}", m.bind, m.port))?;
    let task = tokio::spawn(crucible_meter::serve(listener, cfg));
    let args = vec![
        network_flag.to_owned(),
        m.network.clone(),
        "--model-base-url".into(),
        format!("http://{}:{}/v1", m.bind, m.port),
        "--model".into(),
        model.to_owned(),
    ];
    Ok((SlotMeter { task, log }, args))
}

/// The caps for the next slot run: the taskset's per-stage caps minus what
/// this stage used, and the replica's remaining budget.
fn slot_limits(
    ts: &TaskSet,
    budget: Option<&Budget>,
    stage_used: Used,
    replica_used: Used,
) -> Limits {
    let decl = ts.model.clone().unwrap_or_default();
    let mut l = match budget {
        Some(b) => remaining(b, replica_used),
        None => Limits::default(),
    };
    let min = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    };
    l.max_requests = min(
        l.max_requests,
        decl.max_requests
            .map(|c| c.saturating_sub(stage_used.requests)),
    );
    l.max_tokens = min(
        l.max_tokens,
        decl.max_tokens.map(|c| c.saturating_sub(stage_used.tokens)),
    );
    l
}

fn add_used(a: &mut Used, b: Used) {
    a.requests += b.requests;
    a.tokens += b.tokens;
    a.cost += b.cost;
}

/// Write `opts` (a plugin's options from the taskset) to a temp file.
fn options_file(opts: Option<&serde_json::Value>) -> Result<Option<tempfile::NamedTempFile>> {
    match opts {
        None => Ok(None),
        Some(v) => {
            let f = tempfile::NamedTempFile::new()?;
            std::fs::write(f.path(), serde_json::to_vec(v)?)?;
            Ok(Some(f))
        }
    }
}

/// Run a container plugin's entry script; the scrubbed variables and the
/// other kind's image variable are removed from its environment. It starts
/// its containers with `"$CRUCIBLE" ctr ...` (this program, same backend).
fn plugin_command(p: &ContainerPlugin, image_var: &str, scrub_env: &[String]) -> Command {
    let mut cmd = Command::new("bash");
    if let Some(l) = &p.run_label {
        cmd.env("CRUCIBLE_RUN_LABEL", l);
    }
    if let Ok(me) = std::env::current_exe() {
        cmd.env("CRUCIBLE", me);
    }
    for k in scrub_env {
        cmd.env_remove(k);
    }
    for k in ["CRUCIBLE_SCORER_IMAGE", "CRUCIBLE_RUNNER_IMAGE"] {
        cmd.env_remove(k);
    }
    if let Some(i) = &p.image {
        cmd.env(image_var, i);
    }
    cmd.arg(&p.entry);
    cmd
}

/// What an interactive run left (`run.json`).
#[derive(Deserialize)]
struct RunRecord {
    status: String,
    #[serde(default)]
    detail: String,
}

/// Run the interactive runner; `Err` is a system error of the run.
#[allow(clippy::too_many_arguments)]
fn run_interactive(
    runner: &ContainerPlugin,
    agent: &Path,
    material: &Path,
    out: &Path,
    time_limit_s: u64,
    model_args: &[String],
    options: Option<&Path>,
    scrub_env: &[String],
) -> std::result::Result<(), String> {
    let mut cmd = plugin_command(runner, "CRUCIBLE_RUNNER_IMAGE", scrub_env);
    cmd.arg("--agent")
        .arg(agent)
        .arg("--material")
        .arg(material)
        .arg("--out")
        .arg(out)
        .args(["--time-limit", &time_limit_s.to_string()])
        .args(model_args);
    if let Some(o) = options {
        cmd.arg("--options").arg(o);
    }
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) if s.code() == Some(2) => return Err("runner refused its arguments (exit 2)".into()),
        Ok(s) => return Err(format!("runner wrote no run record ({s})")),
        Err(e) => return Err(format!("could not start the runner: {e}")),
    }
    let rec: RunRecord = std::fs::read(out.join("run.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or("runner run.json is missing or malformed")?;
    match rec.status.as_str() {
        "completed" | "agent_failed" => Ok(()),
        _ => Err(format!(
            "interactive run failed: {}",
            rec.detail.chars().take(200).collect::<String>()
        )),
    }
}

/// Run the scorer on one opened checkpoint.
#[allow(clippy::too_many_arguments)]
fn run_scorer(
    scorer: &ContainerPlugin,
    artifact: &Path,
    tests: &Path,
    stage: &Stage,
    run: Option<&Path>,
    model_args: &[String],
    options: Option<&Path>,
    scrub_env: &[String],
) -> ScoreResult {
    let work = match tempfile::tempdir() {
        Ok(w) => w,
        Err(e) => return system_error(format!("temp dir: {e}")),
    };
    let out = work.path().join("result.json");
    let mut cmd = plugin_command(scorer, "CRUCIBLE_SCORER_IMAGE", scrub_env);
    cmd.arg("--artifact")
        .arg(artifact)
        .arg("--tests")
        .arg(tests)
        .arg("--out")
        .arg(&out)
        .args(["--visibility", "hidden", "--task-id", &stage.id])
        .args(model_args);
    if let Some(r) = run {
        cmd.arg("--run").arg(r);
    }
    if let Some(o) = options {
        cmd.arg("--options").arg(o);
    }
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) if s.code() == Some(2) => {
            return system_error("scorer refused its arguments (exit 2)");
        }
        Ok(s) => return system_error(format!("scorer wrote no result ({s})")),
        Err(e) => return system_error(format!("could not start the scorer: {e}")),
    }
    match std::fs::read(&out)
        .ok()
        .and_then(|b| serde_json::from_slice::<ScoreResult>(&b).ok())
    {
        Some(r) => r,
        None => system_error("scorer result.json is missing or malformed"),
    }
}

/// Everything to score one replica's output of one stage.
struct StageJob<'a> {
    o: &'a ScoreOpts<'a>,
    stage: &'a Stage,
    scorer: &'a ContainerPlugin,
    runner: Option<&'a ContainerPlugin>,
    tests: &'a Path,
}

impl StageJob<'_> {
    /// Score the output `app`; model use is added to `spent` (this
    /// replica) and logged under `usage_dir`.
    async fn score(&self, r: u32, app: &Path, usage_dir: &Path, spent: &mut Used) -> ScoreResult {
        let (o, stage) = (self.o, self.stage);
        let budget = o.model.and_then(|m| m.budgets.get(&r));
        let mut stage_used = Used::default();
        let scrub: Vec<String> = o.scrub_env.to_vec();
        let run_dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(e) => return system_error(format!("temp dir: {e}")),
        };
        let mut have_run = false;
        if let (Some(runner), Some(i)) = (self.runner, &stage.interactive) {
            let model = match slot_model(o.taskset, Slot::Interactive, o.model) {
                Ok(m) => m,
                Err(e) => return rejected(e),
            };
            let mut meter = None;
            let mut model_args = Vec::new();
            if let (Some(name), Some(m)) = (&model, o.model) {
                let limits = slot_limits(o.taskset, budget, stage_used, *spent);
                match start_meter(
                    m,
                    name,
                    usage_dir.join("interactive.jsonl"),
                    limits,
                    "--agent-network",
                )
                .await
                {
                    Ok((mt, a)) => {
                        meter = Some(mt);
                        model_args = a;
                    }
                    Err(e) => return system_error(format!("meter: {e:#}")),
                }
            }
            let opts = match options_file(i.options.as_ref()) {
                Ok(f) => f,
                Err(e) => return system_error(format!("options: {e}")),
            };
            let (rn, agent, material, out) = (
                ContainerPlugin {
                    entry: runner.entry.clone(),
                    image: runner.image.clone(),
                    run_label: runner.run_label.clone(),
                },
                app.to_path_buf(),
                self.tests.to_path_buf(),
                run_dir.path().to_path_buf(),
            );
            let (limit, scrub2, opt_path) = (
                i.time_limit_s,
                scrub.clone(),
                opts.as_ref().map(|f| f.path().to_path_buf()),
            );
            let res = tokio::task::spawn_blocking(move || {
                run_interactive(
                    &rn,
                    &agent,
                    &material,
                    &out,
                    limit,
                    &model_args,
                    opt_path.as_deref(),
                    &scrub2,
                )
            })
            .await
            .unwrap_or_else(|e| Err(format!("runner task: {e}")));
            if let Some(mt) = meter {
                let u = mt.stop().await;
                add_used(&mut stage_used, u);
                add_used(spent, u);
            }
            if let Err(e) = res {
                return system_error(e);
            }
            have_run = true;
        }
        let model = match slot_model(o.taskset, Slot::Scorer, o.model) {
            Ok(m) => m,
            Err(e) => return rejected(e),
        };
        let mut meter = None;
        let mut model_args = Vec::new();
        if let (Some(name), Some(m)) = (&model, o.model) {
            let limits = slot_limits(o.taskset, budget, stage_used, *spent);
            match start_meter(
                m,
                name,
                usage_dir.join("scorer.jsonl"),
                limits,
                "--model-network",
            )
            .await
            {
                Ok((mt, a)) => {
                    meter = Some(mt);
                    model_args = a;
                }
                Err(e) => return system_error(format!("meter: {e:#}")),
            }
        }
        let opts = match options_file(stage.scorer_options.as_ref()) {
            Ok(f) => f,
            Err(e) => return system_error(format!("options: {e}")),
        };
        let sc = ContainerPlugin {
            entry: self.scorer.entry.clone(),
            image: self.scorer.image.clone(),
            run_label: self.scorer.run_label.clone(),
        };
        let (app, tests, st) = (app.to_path_buf(), self.tests.to_path_buf(), stage.clone());
        let run = have_run.then(|| run_dir.path().to_path_buf());
        let opt_path = opts.as_ref().map(|f| f.path().to_path_buf());
        let result = tokio::task::spawn_blocking(move || {
            run_scorer(
                &sc,
                &app,
                &tests,
                &st,
                run.as_deref(),
                &model_args,
                opt_path.as_deref(),
                &scrub,
            )
        })
        .await
        .unwrap_or_else(|e| system_error(format!("scorer task: {e}")));
        if let Some(mt) = meter {
            add_used(spent, mt.stop().await);
        }
        result
    }
}

fn rejected(detail: String) -> ScoreResult {
    ScoreResult {
        visibility: Some("hidden".into()),
        ..ScoreResult::error(ErrorKind::Rejected, detail)
    }
}

/// Score every replica's checkpoint of the selected stages.
pub async fn score(o: &ScoreOpts<'_>) -> Result<Vec<(u32, String, ScoreResult)>> {
    let reps: Vec<u32> = replicas(o.results)?
        .into_iter()
        .filter(|r| o.replica.is_none_or(|x| x == *r))
        .collect();
    if reps.is_empty() {
        bail!("no replica directories under {}", o.results.display());
    }
    let mut spent: BTreeMap<u32, Used> = BTreeMap::new();
    let mut done = Vec::new();
    for &i in &o.stages {
        let stage = &o.taskset.stages[i];
        // Which replicas have anything for this stage at all.
        let todo: Vec<u32> = reps
            .iter()
            .copied()
            .filter(|r| o.results.join(r.to_string()).join(&stage.id).is_dir())
            .collect();
        if todo.is_empty() {
            continue;
        }
        let label = o.run_label.map(str::to_owned);
        let mut scorer = container_plugin(
            o.plugins_root,
            o.taskset,
            Kind::Scorer,
            &crucible_core::taskset::scorer_spec(stage.scorer_ref(o.taskset)),
            "score.sh",
        )
        .await?;
        scorer.run_label = label.clone();
        let runner = match &stage.interactive {
            Some(i) => {
                let mut r =
                    container_plugin(o.plugins_root, o.taskset, Kind::Runner, &i.name, "run.sh")
                        .await?;
                r.run_label = label.clone();
                Some(r)
            }
            None => None,
        };
        // The tests exist in clear only inside this scope.
        let tests = tempfile::tempdir()?;
        let plain = match o.tests {
            TestsFrom::Store(store) => store.get_sealed(&stage.tests_blob, o.keys).await?,
            TestsFrom::Dir(d) => {
                let p = d.join(format!("{}.sealed", stage.id));
                crucible_crypto::open(o.keys, &std::fs::read(&p)?)
                    .with_context(|| format!("opening {}", p.display()))?
            }
        };
        zipdir::safe_extract(Cursor::new(plain), tests.path(), TESTS_LIMITS)
            .with_context(|| format!("stage {} tests", stage.id))?;
        let job = StageJob {
            o,
            stage,
            scorer: &scorer,
            runner: runner.as_ref(),
            tests: tests.path(),
        };
        for r in todo {
            let sdir = o.results.join(r.to_string()).join(&stage.id);
            let dir = o.out.join(r.to_string()).join(&stage.id);
            std::fs::create_dir_all(&dir)?;
            let ckpt = sdir.join("checkpoint.sealed");
            let result = if ckpt.is_file() {
                let app = tempfile::NamedTempFile::new()?;
                let zip = crucible_crypto::open(o.keys, &std::fs::read(&ckpt)?)
                    .with_context(|| format!("opening {}", ckpt.display()))?;
                std::fs::write(app.path(), &zip)?;
                let format =
                    crate::packagers::check(&stage.packager, &zip, stage.packager_options.as_ref());
                drop(zip);
                match format {
                    Ok(()) => {
                        let s = spent.entry(r).or_default();
                        job.score(r, app.path(), &dir.join("eval_usage"), s).await
                    }
                    Err(e) => {
                        eprintln!(
                            "r{r} {}: output refused by packager {}: {e:#}",
                            stage.id, stage.packager
                        );
                        zero("the output is not in the format the stage asks for")
                    }
                }
            } else {
                // The stage ran (it left logs) but produced nothing to score.
                zero("the stage left no checkpoint")
            };
            let result = normalise(result, stage.expected_total, &o.taskset.aggregate);
            std::fs::write(
                dir.join("score.json"),
                serde_json::to_string_pretty(&result)? + "\n",
            )?;
            eprintln!(
                "r{r} {}: {:?} {:?}/{:?}",
                stage.id, result.status, result.score, result.max
            );
            done.push((r, stage.id.clone(), result));
        }
        drop(tests);
    }
    Ok(done)
}

/// Re-seal what the scoring machine needs to a fresh one-run key:
/// `<out>/tests/<stage>.sealed` (the stage's tests zip) and
/// `<out>/results/<replica>/<stage>/checkpoint.sealed` (an empty directory
/// when the stage ran but left no checkpoint), and `<out>/plugins/<id>.sealed`
/// (each uploaded plugin's package). Nothing else of `results` (logs,
/// agent facts) is copied.
pub async fn handoff(
    ts: &TaskSet,
    stages: &[usize],
    results: &Path,
    store: &Store,
    keys: &[PrivateKey],
    out: &Path,
    to: &PublicKey,
) -> Result<()> {
    let reps = replicas(results)?;
    std::fs::create_dir_all(out.join("tests"))?;
    // Uploaded plugins: their packages, built into images on the scoring
    // machine (the package may be private to its uploader).
    for u in &ts.user_plugins {
        let plain = store.get_sealed(&u.blob, keys).await?;
        let d = out.join("plugins");
        std::fs::create_dir_all(&d)?;
        std::fs::write(
            d.join(format!("{}.sealed", u.name)),
            crucible_crypto::seal(to, &plain)?,
        )?;
    }
    for &i in stages {
        let stage = &ts.stages[i];
        let todo: Vec<u32> = reps
            .iter()
            .copied()
            .filter(|r| results.join(r.to_string()).join(&stage.id).is_dir())
            .collect();
        if todo.is_empty() {
            continue;
        }
        let plain = store.get_sealed(&stage.tests_blob, keys).await?;
        std::fs::write(
            out.join("tests").join(format!("{}.sealed", stage.id)),
            crucible_crypto::seal(to, &plain)?,
        )?;
        drop(plain);
        for r in todo {
            let dir = out.join("results").join(r.to_string()).join(&stage.id);
            std::fs::create_dir_all(&dir)?;
            let ckpt = results
                .join(r.to_string())
                .join(&stage.id)
                .join("checkpoint.sealed");
            if ckpt.is_file() {
                let zip = crucible_crypto::open(keys, &std::fs::read(&ckpt)?)
                    .with_context(|| format!("opening {}", ckpt.display()))?;
                std::fs::write(
                    dir.join("checkpoint.sealed"),
                    crucible_crypto::seal(to, &zip)?,
                )?;
            }
        }
    }
    Ok(())
}

/// The whole handoff for `crucible score --tests-dir`: [`handoff`], the
/// taskset, and, when a scoring slot of the taskset uses a model and a
/// credential is given, the credential sealed to `to` (`cred.sealed`) with
/// the model and each replica's remaining budget (`model.json`).
#[allow(clippy::too_many_arguments)]
pub async fn write_handoff(
    ts: &TaskSet,
    taskset_path: &Path,
    stages: &[usize],
    results: &Path,
    store: &Store,
    keys: &[PrivateKey],
    out: &Path,
    to: &PublicKey,
    cred: Option<Credential>,
    model: &str,
    budget: &str,
) -> Result<()> {
    handoff(ts, stages, results, store, keys, out, to).await?;
    std::fs::copy(taskset_path, out.join("taskset.json"))?;
    let (mi, ms) = ts.model_use();
    let wants_model = mi != ModelUse::None || ms != ModelUse::None;
    let had_cred = cred.is_some();
    if let (Some(cred), true) = (cred, wants_model) {
        let model = ts
            .model
            .as_ref()
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| model.trim().to_owned());
        if !crucible_core::taskset::model_name_ok(&model) {
            bail!("a model name is needed (taskset model.name or --model)");
        }
        let budget = Budget::parse(budget)?;
        let mut budgets = BTreeMap::new();
        for r in replicas(results)? {
            let mut u = Used::default();
            for st in &ts.stages {
                let sdir = results.join(r.to_string()).join(&st.id);
                if let (Some(raw), _) = crate::publish::stage_numbers(&sdir, keys)? {
                    add_used(
                        &mut u,
                        used(&crucible_report::usage::parse_jsonl(
                            &String::from_utf8_lossy(&raw),
                        )),
                    );
                }
            }
            let l = remaining(&budget, u);
            budgets.insert(
                r,
                Budget {
                    max_requests: l.max_requests,
                    max_tokens: l.max_tokens,
                    max_cost_usd: l.max_cost_usd,
                    price: None,
                },
            );
        }
        let line = serde_json::to_vec(&serde_json::json!({
            "api_key": cred.api_key,
            "endpoint": cred.endpoint,
        }))?;
        std::fs::write(out.join("cred.sealed"), crucible_crypto::seal(to, &line)?)?;
        drop(line);
        let plan = ModelPlan {
            model,
            budgets,
            price: budget.price.clone(),
        };
        std::fs::write(out.join("model.json"), serde_json::to_vec_pretty(&plan)?)?;
        eprintln!("model credential handed off (sealed to the one-run key)");
    } else if had_cred {
        eprintln!("the taskset gives no scoring slot a model: credential not handed off");
    }
    eprintln!("handoff written for {} stage(s)", stages.len());
    Ok(())
}

/// The model setup of a scoring job from a handoff's `cred.sealed` and
/// `model.json`; the credential is opened only in this process.
pub fn model_setup(
    cred: &Path,
    model_plan: &Path,
    pricing: &Path,
    keys: &[PrivateKey],
    network: &str,
    bind: &str,
    port: u16,
) -> Result<ModelSetup> {
    let plain = crucible_crypto::open(keys, &std::fs::read(cred)?)
        .context("opening the model credential")?;
    let cred: Credential = serde_json::from_slice(&plain)
        .map_err(|_| anyhow::anyhow!("model credential is not {{api_key, endpoint}}"))?;
    drop(plain);
    let plan: ModelPlan = serde_json::from_slice(&std::fs::read(model_plan)?)?;
    let pricing = Pricing::from_json(&std::fs::read_to_string(pricing)?)?;
    let user_price = match &plan.price {
        Some(p) => crucible_metering::parse_price(p)?,
        None => None,
    };
    eprintln!("model credential present: slots that use a model get the meter");
    Ok(ModelSetup {
        cred,
        model: plan.model,
        pricing,
        user_price,
        network: network.to_owned(),
        bind: bind.to_owned(),
        port,
        budgets: plan.budgets,
    })
}

/// `<scores>/<replica>/<stage>/score.json`, if present.
pub fn read_score(scores: &Path, replica: u32, stage: &str) -> Result<Option<ScoreResult>> {
    let p: PathBuf = scores
        .join(replica.to_string())
        .join(stage)
        .join("score.json");
    match std::fs::read(&p) {
        Ok(b) => Ok(Some(
            serde_json::from_slice(&b).with_context(|| format!("parsing {}", p.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old(raw: &str) -> ScoreResult {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn normalise_against_expected_total() {
        use crucible_core::ScoreStatus::{Error, Scored};
        let agg = Aggregate::default();
        let n = normalise(
            old(r#"{"status":"failed","passed":27,"total":30}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.status, n.score, n.max), (Scored, Some(27.0), Some(30.0)));
        // Build failed: no test ran, the stage still counts out of 30.
        let n = normalise(
            old(r#"{"status":"failed","passed":0,"total":0}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.score, n.max), (Some(0.0), Some(30.0)));
        // A pack that collected a different number of tests is flagged.
        let n = normalise(
            old(r#"{"status":"passed","passed":29,"total":29}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.status, n.score, n.max), (Error, None, None));
        assert!(n.detail.contains("expects 30"));
        let n = normalise(
            old(r#"{"status":"system_error","passed":3,"total":4}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.status, n.score), (Error, None));
        let n = normalise(
            old(r#"{"status":"passed","passed":4,"total":4}"#),
            None,
            &agg,
        );
        assert_eq!(
            (n.score, n.max, n.passed),
            (Some(4.0), Some(4.0), Some(true))
        );
        // A continuous score, negative, no max; written as v2.
        let n = normalise(
            old(r#"{"schema":2,"status":"scored","score":-3.25}"#),
            None,
            &agg,
        );
        assert_eq!((n.status, n.score, n.max), (Scored, Some(-3.25), None));
        let json = serde_json::to_string(&n).unwrap();
        assert!(json.contains(r#""schema":2"#) && !json.contains("total"));
    }

    #[tokio::test]
    async fn scores_with_a_stub_scorer() {
        let d = tempfile::tempdir().unwrap();
        let sk = PrivateKey::generate();
        let store = Store::parse(&format!("dir:{}", d.path().join("store").display())).unwrap();
        let (tests_zip, _) = {
            let src = d.path().join("src");
            std::fs::create_dir_all(src.join("tests")).unwrap();
            std::fs::write(src.join("tests/a.spec.ts"), "test").unwrap();
            crate::taskset_cmd::zip_paths(&src, &["tests".into()]).unwrap()
        };
        let tests_blob = store.put_sealed(&sk.public(), &tests_zip).await.unwrap();
        let ts: TaskSet = serde_json::from_value(serde_json::json!({
            "schema": 1, "name": "demo", "scorer": {"name": "astro-survey"}, "total_time_limit_s": 100,
            "stages": [
              {"id": "stage-1", "inputs_blob": tests_blob, "tests_blob": tests_blob, "output": "files", "time_limit_s": 50, "expected_total": 2},
              {"id": "stage-2", "inputs_blob": tests_blob, "tests_blob": tests_blob, "output": "files", "time_limit_s": 50, "expected_total": 2}
            ]
        }))
        .unwrap();
        // Stub scorer in place of astro-survey's: passes 1 of 2 iff the
        // tests and the artifact are there.
        let root = d.path().join("plugins");
        std::fs::create_dir_all(root.join("scorers/astro-survey")).unwrap();
        let scorer = root.join("scorers/astro-survey/score.sh");
        std::fs::write(
            &scorer,
            r#"while [ $# -gt 0 ]; do case "$1" in --artifact) A=$2;; --tests) T=$2;; --out) O=$2;; esac; shift 2; done
[ -f "$T/tests/a.spec.ts" ] && grep -q app.txt "$A" || exit 1
printf '{"visibility":"hidden","status":"failed","passed":1,"total":2,"detail":"1/2 tests failed"}' > "$O""#,
        )
        .unwrap();
        let results = d.path().join("results");
        let s1 = results.join("1/stage-1");
        std::fs::create_dir_all(&s1).unwrap();
        let app = {
            let w = d.path().join("work");
            std::fs::create_dir_all(&w).unwrap();
            std::fs::write(w.join("app.txt"), "x").unwrap();
            crate::packagers::package("files", &w, &[], None).unwrap().0
        };
        std::fs::write(
            s1.join("checkpoint.sealed"),
            crucible_crypto::seal(&sk.public(), &app).unwrap(),
        )
        .unwrap();
        // Replica 2 uploaded something that is not a zip at all: 0, the
        // scorer never runs.
        let s2 = results.join("2/stage-1");
        std::fs::create_dir_all(&s2).unwrap();
        std::fs::write(
            s2.join("checkpoint.sealed"),
            crucible_crypto::seal(&sk.public(), b"not a zip").unwrap(),
        )
        .unwrap();
        // Stage 2 ran but left only logs.
        std::fs::create_dir_all(results.join("1/stage-2")).unwrap();
        let out = d.path().join("scores");
        let done = score(&ScoreOpts {
            taskset: &ts,
            stages: vec![0, 1],
            results: &results,
            out: &out,
            tests: TestsFrom::Store(&store),
            keys: std::slice::from_ref(&sk),
            plugins_root: &root,
            scrub_env: &[],
            replica: None,
            model: None,
            run_label: None,
        })
        .await
        .unwrap();
        assert_eq!(done.len(), 3);
        let s1 = read_score(&out, 1, "stage-1").unwrap().unwrap();
        assert_eq!(
            (s1.score, s1.max, s1.passed),
            (Some(1.0), Some(2.0), Some(false))
        );
        let s2 = read_score(&out, 1, "stage-2").unwrap().unwrap();
        assert_eq!(
            (s2.status.is_scored(), s2.score, s2.max),
            (true, Some(0.0), Some(2.0))
        );
        let bad = read_score(&out, 2, "stage-1").unwrap().unwrap();
        assert_eq!((bad.score, bad.max), (Some(0.0), Some(2.0)));
        assert!(read_score(&out, 3, "stage-1").unwrap().is_none());

        // Hand off to a machine without the platform key: same scores with
        // only the one-run key, and the scorer never sees the key variable.
        let hand = d.path().join("handoff");
        let run_key = PrivateKey::generate();
        handoff(
            &ts,
            &[0, 1],
            &results,
            &store,
            std::slice::from_ref(&sk),
            &hand,
            &run_key.public(),
        )
        .await
        .unwrap();
        assert!(hand.join("tests/stage-1.sealed").is_file());
        assert!(hand.join("results/1/stage-2").is_dir());
        assert!(!hand.join("results/1/stage-2/checkpoint.sealed").exists());
        assert!(
            crucible_crypto::open(
                std::slice::from_ref(&sk),
                &std::fs::read(hand.join("tests/stage-1.sealed")).unwrap()
            )
            .is_err()
        );
        std::fs::write(
            &scorer,
            format!(
                "[ -z \"${{PATH_CRUCIBLE_TEST_KEY:-}}\" ] || exit 1\n{}",
                std::fs::read_to_string(&scorer).unwrap()
            ),
        )
        .unwrap();
        let out2 = d.path().join("scores2");
        let keys = [run_key];
        // SAFETY: test-only; nothing else reads this variable.
        unsafe { std::env::set_var("PATH_CRUCIBLE_TEST_KEY", "x") };
        score(&ScoreOpts {
            taskset: &ts,
            stages: vec![0, 1],
            results: &hand.join("results"),
            out: &out2,
            tests: TestsFrom::Dir(&hand.join("tests")),
            keys: &keys,
            plugins_root: &root,
            scrub_env: &["PATH_CRUCIBLE_TEST_KEY".into()],
            replica: Some(1),
            model: None,
            run_label: None,
        })
        .await
        .unwrap();
        let s1 = read_score(&out2, 1, "stage-1").unwrap().unwrap();
        assert_eq!((s1.score, s1.max), (Some(1.0), Some(2.0)));
        let s2 = read_score(&out2, 1, "stage-2").unwrap().unwrap();
        assert_eq!((s2.score, s2.max), (Some(0.0), Some(2.0)));
    }
}
