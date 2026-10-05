//! `crucible step generate`: one replica of an agent run (eval.yml's
//! `generate` job). Fetch and build the agent (no secret used yet, except
//! the platform key for a sealed package), fetch the stage inputs (never
//! the tests), bring up the sandbox network, open the model credential in
//! this process, run the stages, and seal every output to the platform
//! key. Only sealed files are left in `<out>/<replica>/`, whatever
//! happened; a failed run still seals what it left.
//!
//! On failure the public log and the step's error carry only a category
//! ([`FETCH`], [`BUILD`], [`SETUP`], [`RUN`]); the details (error chain,
//! the tail of the agent's image build log) go into `failure.json`, sealed
//! with the agent facts in `agent.sealed`, and from there into the
//! manifest's `failure_detail`, which only the submitter sees.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::{Common, Secret, Secrets};
use crate::agentpkg;
use crate::build;
use crate::cred::{self, CredSource};
use crate::executor::Executor;
use crate::runners;

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: Common,
    /// Holds agents/ (builtin agents) and config/ (pricing, egress
    /// allowlist, keys); must hold no test material.
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// taskset.json.
    #[arg(long)]
    pub taskset: PathBuf,
    #[arg(long)]
    pub agent_source: String,
    /// Builtin agents built from another repo: branch or tag to build.
    #[arg(long, default_value = "")]
    pub agent_ref: String,
    #[arg(long)]
    pub model: String,
    /// Overrides the credential's endpoint.
    #[arg(long, default_value = "")]
    pub endpoint: String,
    #[arg(long, default_value = "")]
    pub budget: String,
    #[arg(long)]
    pub stages: usize,
    #[arg(long, default_value_t = 1)]
    pub replica: u32,
    /// `github-secret` (DevModelCred, sealed to the platform key) or `workers-kv`.
    #[arg(long)]
    pub cred_source: String,
    #[arg(long, default_value = "")]
    pub eval_id: String,
    /// The Worker (workers-kv credentials, progress reports).
    #[arg(long, default_value = "")]
    pub worker_url: String,
    /// Report `running:<stage>` to the Worker (replica 1 only).
    #[arg(long)]
    pub report_progress: bool,
    #[arg(long)]
    pub store: String,
    /// Upper bound of the stage run, minutes.
    #[arg(long, default_value_t = 330)]
    pub run_timeout_min: u64,
    #[arg(long, default_value = "crucible-agent:run")]
    pub image_tag: String,
    /// Scratch directory (agent package, inputs, work dir; emptied at the
    /// end); default: a temp dir.
    #[arg(long)]
    pub work: Option<PathBuf>,
    /// Label of everything this step starts (`crucible.run=<label>`); what
    /// an earlier attempt with the same label left is removed first.
    /// Default: `<eval id>-r<replica>`.
    #[arg(long)]
    pub run_label: Option<String>,
    /// Output bundle: `<out>/<replica>/...`, sealed files only.
    #[arg(long)]
    pub out: PathBuf,
}

/// Failure categories (the manifest's `replicas[].failure`).
pub const FETCH: &str = "agent package could not be fetched";
pub const BUILD: &str = "agent image build failed";
pub const SETUP: &str = "platform setup failed";
pub const RUN: &str = "agent run failed";

/// At most this much of the details is kept (the end of it).
const MAX_DETAIL: usize = 6000;

pub async fn run(a: Args, s: &Secrets) -> Result<()> {
    let tmp;
    let work = match &a.work {
        Some(w) => {
            std::fs::create_dir_all(w)?;
            std::fs::canonicalize(w)?
        }
        None => {
            tmp = tempfile::tempdir()?;
            tmp.path().to_path_buf()
        }
    };
    let rdir = work.join("run").join(a.replica.to_string());
    std::fs::create_dir_all(&rdir)?;
    let label = run_label(&a);
    let exec = crate::executor::backend()?;
    // Scoped: only what carries this run's label.
    exec.cleanup(&label);
    let phase = Cell::new(SETUP);
    let result = generate(&a, s, &work, &rdir, &label, &phase).await;
    exec.cleanup(&label);
    exec.image_rm(&a.image_tag).await;
    // Public log: the category only; details are sealed.
    let result = result.map_err(|e| {
        let category = phase.get();
        let detail = format!("{e:#}");
        let cut = detail.len().saturating_sub(MAX_DETAIL);
        let cut = (cut..=detail.len())
            .find(|&i| detail.is_char_boundary(i))
            .unwrap_or(0);
        let f = serde_json::json!({"category": category, "detail": &detail[cut..]});
        if let Err(w) = std::fs::write(work.join("failure.json"), f.to_string()) {
            eprintln!("could not record the failure: {w}");
        }
        eprintln!("generate r{}: {category}", a.replica);
        anyhow!("{category}")
    });
    // Seal whatever the run left, whatever happened.
    let out = a.out.join(a.replica.to_string());
    let extra: Vec<PathBuf> = ["facts.json", "build.json", "failure.json"]
        .iter()
        .map(|f| work.join(f))
        .filter(|p| p.is_file())
        .collect();
    let key = crate::keys::current_public_key(&a.root.join("config/keys.json"))?;
    for (stage, what, sha) in crate::publish::seal_outputs(&rdir, &key, &out, &extra)? {
        eprintln!("sealed {stage}/{what}: {sha}");
    }
    // Only sealed files leave this machine.
    super::sealed_only(&a.out, true)?;
    // Inputs and the work tree are not needed once sealed.
    if a.work.is_some() {
        for e in std::fs::read_dir(&work)?.flatten() {
            let _ = std::fs::remove_dir_all(e.path()).or_else(|_| std::fs::remove_file(e.path()));
        }
    }
    result
}

fn run_label(a: &Args) -> String {
    match &a.run_label {
        Some(l) if !l.is_empty() => l.clone(),
        _ if crate::plan::eval_id_ok(&a.eval_id) => format!("{}-r{}", a.eval_id, a.replica),
        _ => format!("gen-{}-r{}", std::process::id(), a.replica),
    }
}

/// `phase` is the failure category of what is being done.
async fn generate(
    a: &Args,
    s: &Secrets,
    work: &Path,
    rdir: &Path,
    label: &str,
    phase: &Cell<&'static str>,
) -> Result<()> {
    if super::has_test_material(&a.root)? {
        bail!("test material present in the generation step's root");
    }
    let ts = crate::taskset_cmd::load(&a.taskset)?;
    ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
    let store = s.store(&a.store, false)?;

    // The agent package.
    phase.set(FETCH);
    let src = agentpkg::Source::parse(&a.agent_source)?;
    let pkg = work.join("agent-pkg");
    let blob_access = match &src {
        agentpkg::Source::Blob(_) => {
            Some((s.store(&a.store, false)?, s.keys(Secret::PlatformKey)?))
        }
        _ => None,
    };
    let facts = agentpkg::fetch(&src, &a.root.join("agents"), &pkg, blob_access.as_ref()).await?;
    std::fs::write(work.join("facts.json"), serde_json::to_vec(&facts)?)?;
    if src.kind() == "blob" {
        eprintln!("agent package fetched");
    } else {
        let v = serde_json::to_value(&facts)?;
        eprintln!(
            "{}",
            serde_json::json!({
                "source_kind": v["source_kind"], "name": v["agent"]["name"],
                "version": v["agent"]["version"], "commit": v["commit"],
            })
        );
    }

    // Build (nothing from the package runs on the host).
    phase.set(BUILD);
    let mut opts = build::BuildOpts {
        log: Some(work.join("build.log")),
        timeout: Some(Duration::from_secs(3600)),
        ..Default::default()
    };
    let up = pkg.join("upstream.json");
    if src.kind() == "builtin" && up.is_file() {
        // A builtin agent built from another repo: build exactly the commit
        // its branch points at now (never a moving "main").
        let u: serde_json::Value = serde_json::from_slice(&std::fs::read(&up)?)?;
        let field = |k: &str| {
            u[k].as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("upstream.json: {k} missing"))
        };
        let (repo, arg) = (field("repo")?, field("build_arg")?);
        let r#ref = if a.agent_ref.is_empty() {
            field("ref")?
        } else {
            a.agent_ref.clone()
        };
        let commit = ls_remote(&repo, &r#ref)?;
        eprintln!("agent source: {repo} {} = {commit}", r#ref);
        opts.build_args = vec![format!("{arg}={commit}")];
        opts.expect_commit = Some(commit);
    }
    let tag = a.image_tag.clone();
    let built = match crate::executor::backend()? {
        crate::executor::Backend::K8s(k) => build::build_k8s(&k, &pkg, &tag, &opts).await,
        crate::executor::Backend::Docker(_) => {
            let pkg2 = pkg.clone();
            tokio::task::spawn_blocking(move || build::build(&pkg2, &tag, &opts)).await?
        }
    };
    let facts = match built {
        Ok(f) => f,
        Err(e) => {
            // The submitter's own build output: sealed, never printed.
            let log = std::fs::read(work.join("build.log")).unwrap_or_default();
            let tail = String::from_utf8_lossy(&log[log.len().saturating_sub(4000)..]);
            bail!("{e:#}\n--- build log (end) ---\n{tail}");
        }
    };
    std::fs::write(work.join("build.json"), serde_json::to_vec(&facts)?)?;
    eprintln!(
        "{}",
        serde_json::json!({"size_bytes": facts.size_bytes, "commit": facts.agent_build["commit"]})
    );

    // Stage inputs, never the tests.
    phase.set(SETUP);
    let keys = s.keys(Secret::PlatformKey)?;
    let inputs = work.join("inputs");
    crate::taskset_cmd::fetch_inputs(&ts, a.stages, &store, &keys, &inputs).await?;
    if super::has_test_material(&inputs)? {
        bail!("test material in the stage inputs");
    }

    let exec = crate::executor::backend()?;
    super::check_caps(super::spec("generate"), exec.caps())?;
    let net = exec.sandbox(&[8787, 3128], label)?;

    // The model credential: opened here, kept in this process only.
    let src = CredSource::parse(&a.cred_source)?;
    let (sealed, expect) = match src {
        CredSource::GithubSecret => (s.bytes(Secret::DevModelCred)?.to_vec(), None),
        CredSource::WorkersKv => {
            if a.worker_url.is_empty() || a.eval_id.is_empty() {
                bail!("workers-kv needs --worker-url and --eval-id");
            }
            (
                s.worker(&a.worker_url)?.get_cred(&a.eval_id).await?,
                Some(a.eval_id.as_str()),
            )
        }
    };
    let endpoint = Some(a.endpoint.as_str()).filter(|e| !e.is_empty());
    let line = cred::open_credential(&sealed, &keys, endpoint, expect)?;
    drop(keys);
    let cred = crucible_meter::read_credential(&mut line.as_bytes()).map_err(|e| anyhow!("{e}"))?;
    drop(line);

    let args = runners::RunArgs {
        image: a.image_tag.clone(),
        agent_json: pkg.join("agent.json"),
        taskset: a.taskset.clone(),
        inputs_dir: inputs,
        stages: Some(a.stages),
        model: a.model.clone(),
        out_dir: rdir.to_path_buf(),
        scratch_dir: work.join("scratch"),
        pricing: a.root.join("config/pricing.json"),
        egress_allow: a.root.join("config/egress.json"),
        bind: net.host.clone(),
        agent_host: None,
        meter_port: 8787,
        egress_port: 3128,
        network: net.network.clone(),
        budget: a.budget.clone(),
        snapshot_interval_s: 900,
        grace_s: 30,
        user: None,
        run_label: label.to_owned(),
    };
    // Progress: replica 1 reports running:<stage> as each stage starts.
    let reporter = match (a.report_progress && a.replica == 1, a.worker_url.is_empty()) {
        (true, false) => Some(Arc::new(s.worker(&a.worker_url)?)),
        _ => None,
    };
    let eval_id = a.eval_id.clone();
    let on_stage = move |stage: &str| {
        if let Some(w) = reporter.clone() {
            let (id, st) = (eval_id.clone(), format!("running:{stage}"));
            tokio::spawn(async move {
                if let Err(e) = w.status(&id, &st).await {
                    println!("::warning::status {st} not reported: {e:#}");
                }
            });
        }
    };
    phase.set(RUN);
    let r = tokio::time::timeout(
        Duration::from_secs(a.run_timeout_min * 60),
        runners::run_with(&exec, &args, &cred, &on_stage),
    )
    .await;
    drop(cred);
    exec.cleanup(label);
    drop(net);
    match r {
        Ok(r) => r,
        Err(_) => bail!("the stages ran past {} minutes", a.run_timeout_min),
    }
}

/// The full commit `r#ref` of `repo` points at.
fn ls_remote(repo: &str, r#ref: &str) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["ls-remote", "--", repo, r#ref])
        .stdin(std::process::Stdio::null())
        .output()
        .context("git ls-remote")?;
    let commit = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.split('\t').next())
        .unwrap_or_default()
        .to_owned();
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("cannot resolve {} of {repo}", r#ref);
    }
    Ok(commit)
}
