//! `crucible eval local`: one evaluation on this machine, no GitHub, no
//! Worker (docs/executors.md §5.2). The driver runs the same steps the
//! workflows run, each as its own `crucible step` process, in order:
//! generate (per replica) -> handoff -> score-tests (per replica) ->
//! publish. Bundles are directories under `<out>/<eval id>/`.
//!
//! Keys: a local key pair (`crucible keys gen`, default `~/.crucible/keys`)
//! stands in for the platform key; the taskset is packed from its public
//! source with it into the local store. Official tasksets sealed to the
//! platform key are not used, and results are this machine's own (never a
//! leaderboard). The one-run key of the handoff is generated here: its
//! public half goes to `handoff`, its private half only to `score-tests`.
//!
//! Secrets reach each step as files in a fresh directory (on /dev/shm when
//! there is one) holding exactly the step's list, deleted when it exits.
//!
//! Several evaluations can run at once: each step takes a free sandbox
//! slot, the agent image is tagged per evaluation, containers are labelled
//! per run, and all of them are removed at the end.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, bail};
use crucible_crypto::PrivateKey;

use crate::keys::Store;
use crate::steps::{Secret, spec};

#[derive(clap::Subcommand)]
pub enum EvalCmd {
    /// Run an evaluation on this machine (Linux, Docker, passwordless
    /// `sudo iptables`): an agent (`--agent`) or an uploaded output (`--app`).
    Local(Box<LocalArgs>),
    /// The same, each step a Nomad batch job (docs/nomad.md).
    Nomad(Box<crate::nomad::NomadArgs>),
    /// The same, each step a Kubernetes Job, containers as Pods
    /// (docs/kubernetes.md).
    K8s(Box<crate::k8s_eval::K8sArgs>),
}

#[derive(clap::Args)]
pub struct LocalArgs {
    /// The crucible repository (agents/, config/, scorers/, runners/,
    /// tasksets/, tools/).
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// A taskset under <root>/tasksets/ by name, or a directory holding
    /// source.json (its stage dirs next to it, or under source/).
    #[arg(long)]
    pub taskset: String,
    /// Agent mode: builtin:<name> | url:<https zip> | git:<https url>@<ref>.
    #[arg(long, conflicts_with_all = ["app", "stage"], required_unless_present = "app")]
    pub agent: Option<String>,
    /// App mode: an output zip to score on `--stage`.
    #[arg(long, requires = "stage")]
    pub app: Option<PathBuf>,
    /// App mode: the stage (1-based).
    #[arg(long)]
    pub stage: Option<usize>,
    /// Agent mode: run the first N stages.
    #[arg(long)]
    pub stages: Option<usize>,
    #[arg(long, default_value_t = 1)]
    pub replicas: u32,
    /// The model the agent (or a scoring slot that uses one) calls.
    #[arg(long, default_value = "")]
    pub model: String,
    /// OpenAI-compatible base URL; overrides the credential's.
    #[arg(long, default_value = "")]
    pub endpoint: String,
    /// The model credential, JSON {"api_key", "endpoint"} (`-`: stdin).
    /// Needed for agent mode; in app mode only for scoring slots that use
    /// a model.
    #[arg(long)]
    pub cred_file: Option<String>,
    /// Run-wide caps, JSON {"max_requests","max_tokens","max_cost_usd","price"}.
    #[arg(long, default_value = "")]
    pub budget: String,
    /// Local key pair directory (`crucible keys gen`); created when missing.
    #[arg(long)]
    pub keys: Option<PathBuf>,
    /// Blob store; default dir:~/.crucible/store.
    #[arg(long)]
    pub store: Option<String>,
    /// Evaluations are written to <out>/<eval id>/.
    #[arg(long, default_value = "crucible-evals")]
    pub out: PathBuf,
    #[arg(long, default_value = "")]
    pub eval_id: String,
}

#[derive(clap::Subcommand)]
pub enum KeysCmd {
    /// Generate a local key pair: <out>/private.key (0600) and
    /// <out>/keys.json (config/keys.json format).
    Gen {
        #[arg(long)]
        out: PathBuf,
    },
}

pub fn keys_gen(out: &Path) -> Result<()> {
    if out.join("private.key").exists() {
        bail!("{} already holds a key", out.display());
    }
    std::fs::create_dir_all(out)?;
    let k = PrivateKey::generate();
    let public = k.public();
    crate::write_private(&out.join("private.key"), k.to_secret_string().as_bytes())?;
    let id = public.key_id();
    std::fs::write(
        out.join("keys.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "current": id,
            "keys": [{"key_id": id, "public_key": public.to_string(), "private_key_locations": ["local"]}],
        }))? + "\n",
    )?;
    eprintln!("local key {id} written to {}", out.display());
    Ok(())
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

/// One step's secrets directory; removed when dropped.
struct SecretsDir(PathBuf);

impl SecretsDir {
    fn new(base: &Path, step: &str, vals: &[(Secret, &[u8])]) -> Result<SecretsDir> {
        let d = base.join(format!("{step}-{}", rand_hex(6)));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&d)?;
        let dir = SecretsDir(d);
        let allowed = spec(step).secrets;
        for (s, v) in vals {
            if !allowed.contains(s) {
                bail!("step {step} may not hold {}", s.name());
            }
            crate::write_private(&dir.0.join(s.name()), v)?;
        }
        Ok(dir)
    }
}

impl Drop for SecretsDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rand_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::getrandom(&mut b).expect("random");
    hex::encode(b)
}

/// The step record the driver keeps (its own clock is the time source).
#[derive(serde::Serialize)]
struct StepRecord {
    step: String,
    started_at: String,
    ended_at: String,
    wall_s: f64,
    ok: bool,
}

/// Who runs the steps.
pub enum Backend {
    /// Child processes of the driver.
    Local,
    /// One Nomad batch job per step.
    Nomad(crate::nomad::Nomad),
    /// One Kubernetes Job per step, in the evaluation's namespace.
    K8s(Box<crate::k8s_eval::K8s>),
}

struct Driver {
    backend: Backend,
    eval_id: String,
    /// The evaluation directory (copied to the Kubernetes volume).
    dir: PathBuf,
    secrets_base: PathBuf,
    records: Vec<StepRecord>,
}

impl Driver {
    /// Run `crucible step <args>` with exactly `secrets`.
    async fn step(
        &mut self,
        exe: &Path,
        name: &str,
        label: &str,
        secrets: &[(Secret, &[u8])],
        args: &[String],
    ) -> Result<bool> {
        eprintln!("== step {label}");
        if let Backend::K8s(k) = &mut self.backend {
            k.prepare(&self.dir).await?;
        }
        let (ok, started, ended) = match &self.backend {
            Backend::K8s(k) => {
                let e = k
                    .run_step(label, spec(name), &exe.display().to_string(), args, secrets)
                    .await?;
                (e.ok, e.started, e.ended)
            }
            Backend::Local => self.local_step(exe, name, secrets, args).await?,
            Backend::Nomad(n) => {
                let env: Vec<(String, String)> = if name == "score-tests" {
                    vec![("CRUCIBLE_SCORER_FIREWALL".into(), "1".into())]
                } else {
                    vec![]
                };
                let id = crate::nomad::job_id(&self.eval_id, label);
                let e = n
                    .run_step(&id, spec(name), exe, args, &env, secrets)
                    .await?;
                (e.ok, e.started, e.ended)
            }
        };
        self.records.push(StepRecord {
            step: label.into(),
            started_at: humantime::format_rfc3339_seconds(started).to_string(),
            ended_at: humantime::format_rfc3339_seconds(ended).to_string(),
            wall_s: ended
                .duration_since(started)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            ok,
        });
        if !ok {
            eprintln!("== step {label} failed");
        }
        Ok(ok)
    }

    /// A child process with a secrets directory removed when it exits.
    async fn local_step(
        &self,
        exe: &Path,
        name: &str,
        secrets: &[(Secret, &[u8])],
        args: &[String],
    ) -> Result<(bool, SystemTime, SystemTime)> {
        let dir = SecretsDir::new(&self.secrets_base, name, secrets)?;
        let started = SystemTime::now();
        let mut cmd = tokio::process::Command::new(exe);
        cmd.arg("step")
            .arg(name)
            .arg("--secrets-dir")
            .arg(&dir.0)
            .args(args);
        for k in crate::steps::SECRET_ENVS {
            cmd.env_remove(k);
        }
        if name == "score-tests" {
            cmd.env("CRUCIBLE_SCORER_FIREWALL", "1");
        }
        let status = cmd
            .status()
            .await
            .with_context(|| format!("starting step {name}"))?;
        drop(dir);
        Ok((status.success(), started, SystemTime::now()))
    }
}

/// Copy a directory tree (regular files and directories; links are
/// copied as the files they point to).
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if std::fs::metadata(&src)?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Pack a taskset source with `keys_json`'s key into `store`; returns
/// the taskset.
async fn pack_taskset(
    root: &Path,
    taskset: &str,
    keys_json: &Path,
    store: &Store,
) -> Result<crucible_core::TaskSet> {
    let dir = if crucible_core::is_slug(taskset, 64) && root.join("tasksets").join(taskset).is_dir()
    {
        root.join("tasksets").join(taskset)
    } else {
        PathBuf::from(taskset)
    };
    let source = dir.join("source.json");
    if !source.is_file() {
        bail!("{} has no source.json", dir.display());
    }
    let src_dir = if dir.join("source").is_dir() {
        dir.join("source")
    } else {
        dir.clone()
    };
    let key = crate::keys::current_public_key(keys_json)?;
    crate::taskset_cmd::pack(&source, &src_dir, false, &key, store, false).await
}

pub async fn eval_local(a: LocalArgs) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("eval local needs Linux (Docker with iptables); see docs/executors.md");
    }
    crate::executor::docker::check_runtime()?;
    if let Some(r) = crate::executor::docker::runtime() {
        eprintln!("containers run under the {r} runtime (CRUCIBLE_DOCKER_RUNTIME)");
    }
    let exe = std::env::current_exe()?;
    run_eval(a, Backend::Local, exe).await
}

pub async fn eval_nomad(a: crate::nomad::NomadArgs) -> Result<()> {
    let n = crate::nomad::Nomad::new(&a)?;
    let exe = match &a.crucible_bin {
        Some(p) => std::path::absolute(p)?,
        None => std::env::current_exe()?,
    };
    run_eval(a.eval, Backend::Nomad(n), exe).await
}

pub async fn eval_k8s(a: crate::k8s_eval::K8sArgs) -> Result<()> {
    if a.eval.store.is_some() {
        bail!("eval k8s keeps each evaluation's store on its volume: no --store");
    }
    let exe = match &a.crucible_bin {
        Some(p) => std::path::absolute(p)?,
        None => std::env::current_exe()?,
    };
    let k = crate::k8s_eval::K8s::new(&a)?;
    run_eval(a.eval, Backend::K8s(Box::new(k)), exe).await
}

/// The driver: plan, generate per replica, handoff, score-tests per
/// replica, publish, each step run by `backend`.
async fn run_eval(a: LocalArgs, mut backend: Backend, exe: PathBuf) -> Result<()> {
    let (prefix, timing_source) = match backend {
        Backend::Local => ("local", "local"),
        Backend::Nomad(_) => ("nomad", "nomad"),
        Backend::K8s(_) => ("k8s", "k8s"),
    };
    let k8s = matches!(backend, Backend::K8s(_));
    let root =
        std::fs::canonicalize(&a.root).with_context(|| format!("--root {}", a.root.display()))?;
    let keys_dir = match &a.keys {
        Some(k) => k.clone(),
        None => home()?.join(".crucible/keys"),
    };
    if !keys_dir.join("private.key").is_file() {
        keys_gen(&keys_dir)?;
    }
    let private = std::fs::read(keys_dir.join("private.key"))?;
    let key = PrivateKey::parse(std::str::from_utf8(&private)?)?;
    let public = key.public();
    let eval_id = match a.eval_id.trim() {
        "" => {
            let t = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
            let t: String = t.chars().filter(char::is_ascii_digit).take(14).collect();
            format!("{prefix}-{t}-{}", rand_hex(3))
        }
        id if crate::plan::eval_id_ok(id) => id.to_owned(),
        _ => bail!("--eval-id must match [a-z0-9][a-z0-9-]{{7,63}}"),
    };
    let dir = std::path::absolute(a.out.join(&eval_id))?;
    if dir.exists() {
        bail!("{} exists", dir.display());
    }
    std::fs::create_dir_all(&dir)?;
    eprintln!("eval {eval_id}: {}", dir.display());
    // Paths as the steps see them: Kubernetes steps see the evaluation
    // directory at /crucible (its volume).
    let remote = k8s.then(|| PathBuf::from(crate::k8s_eval::ROOT));
    let s = |x: &Path| match &remote {
        Some(r) => r
            .join(x.strip_prefix(&dir).unwrap_or(x))
            .display()
            .to_string(),
        None => x.display().to_string(),
    };
    let (store_spec, local_store) = match (&a.store, k8s) {
        (_, true) => (
            format!("dir:{}", s(&dir.join("store"))),
            format!("dir:{}", dir.join("store").display()),
        ),
        (Some(x), false) => (x.clone(), x.clone()),
        (None, false) => {
            let d = format!("dir:{}", home()?.join(".crucible/store").display());
            (d.clone(), d)
        }
    };
    if !store_spec.starts_with("dir:") {
        bail!("eval local stores on this machine: --store dir:<path>");
    }
    let store = Store::parse(&local_store)?;
    let exe = if let Backend::K8s(k) = &mut backend {
        k.bind(&eval_id);
        std::fs::create_dir_all(dir.join("bin"))?;
        std::fs::copy(&exe, dir.join("bin/crucible"))?;
        PathBuf::from(s(&dir.join("bin/crucible")))
    } else {
        exe
    };

    // The steps' root: the repository's plugins and agents, the local key.
    let sroot = dir.join("root");
    std::fs::create_dir_all(sroot.join("config"))?;
    for d in ["agents", "scorers", "runners", "tools"] {
        if k8s {
            // The volume gets copies (the repository is not there).
            copy_dir(&root.join(d), &sroot.join(d))?;
        } else {
            std::os::unix::fs::symlink(root.join(d), sroot.join(d))?;
        }
    }
    for f in ["pricing.json", "egress.json"] {
        std::fs::copy(root.join("config").join(f), sroot.join("config").join(f))?;
    }
    std::fs::copy(keys_dir.join("keys.json"), sroot.join("config/keys.json"))?;
    // generate's root: agents and config only. Locally the plugin
    // directories are links it does not follow; on a volume they would be
    // copies, and a scorer's own test fixtures would count as test
    // material on the generation machine.
    let gen_root = if k8s {
        let g = dir.join("root-gen");
        copy_dir(&sroot.join("agents"), &g.join("agents"))?;
        copy_dir(&sroot.join("config"), &g.join("config"))?;
        g
    } else {
        sroot.clone()
    };
    let ts = pack_taskset(&root, &a.taskset, &sroot.join("config/keys.json"), &store).await?;
    let ts_dir = sroot.join("tasksets").join(&ts.name);
    std::fs::create_dir_all(&ts_dir)?;
    let ts_path = ts_dir.join("taskset.json");
    std::fs::write(&ts_path, serde_json::to_string_pretty(&ts)? + "\n")?;
    eprintln!(
        "taskset {}: {} stages, packed with local key {}",
        ts.name,
        ts.stages.len(),
        public.key_id()
    );

    // The model credential, sealed to the local key (the dev credential path).
    let cred = match &a.cred_file {
        None => None,
        Some(f) => {
            let raw = crate::read_input(f)?;
            let c =
                crucible_meter::read_credential(&mut raw.as_slice()).map_err(|e| anyhow!("{e}"))?;
            let line = serde_json::to_vec(
                &serde_json::json!({"api_key": c.api_key, "endpoint": c.endpoint}),
            )?;
            Some(crucible_crypto::seal(&public, &line)?)
        }
    };
    let secrets_base = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        dir.clone()
    };
    let mut drv = Driver {
        backend,
        eval_id: eval_id.clone(),
        dir: dir.clone(),
        secrets_base,
        records: Vec::new(),
    };
    let pk: &[u8] = &private;
    let ts_s = s(&ts_path);
    let (gens, scores, handoff, publish) = (
        dir.join("gen"),
        dir.join("scores"),
        dir.join("handoff"),
        dir.join("publish"),
    );
    let run_key = PrivateKey::generate();
    let run_secret = run_key.to_secret_string();

    let result: Result<()> = async {
        let (mode_args, replicas) = match (&a.agent, &a.app) {
            (Some(agent), _) => {
                let Some(c) = &cred else {
                    bail!("agent mode needs --cred-file");
                };
                // Same checks as a workflow dispatch.
                let plan = crate::plan::plan(
                    &crate::plan::PlanInputs {
                        agent_source: agent.clone(),
                        taskset: ts.name.clone(),
                        model: a.model.clone(),
                        endpoint: a.endpoint.clone(),
                        replicas: a.replicas.to_string(),
                        cred_source: "github-secret".into(),
                        budget: a.budget.clone(),
                        stages: a.stages.map(|n| n.to_string()).unwrap_or_default(),
                        eval_id: eval_id.clone(),
                        score_public: "false".into(),
                        run_attempt: "1".into(),
                        ..Default::default()
                    },
                    &sroot,
                )?;
                let o = |k: &str| plan.get(k).cloned().unwrap_or_default();
                for r in 1..=a.replicas {
                    let tag = format!("crucible-agent-{eval_id}:r{r}");
                    drv.step(
                        &exe,
                        "generate",
                        &format!("generate r{r}"),
                        &[(Secret::PlatformKey, pk), (Secret::DevModelCred, c)],
                        &[
                            "--root".into(),
                            s(&gen_root),
                            "--taskset".into(),
                            ts_s.clone(),
                            "--agent-source".into(),
                            o("agent_source"),
                            "--agent-ref".into(),
                            o("agent_ref"),
                            "--model".into(),
                            o("model"),
                            "--endpoint".into(),
                            o("endpoint"),
                            "--budget".into(),
                            o("budget"),
                            "--stages".into(),
                            o("stages"),
                            "--replica".into(),
                            r.to_string(),
                            "--cred-source".into(),
                            "github-secret".into(),
                            "--eval-id".into(),
                            eval_id.clone(),
                            "--store".into(),
                            store_spec.clone(),
                            "--run-timeout-min".into(),
                            o("run_timeout_min"),
                            "--image-tag".into(),
                            tag.clone(),
                            "--work".into(),
                            s(&dir.join(format!("work-r{r}"))),
                            "--out".into(),
                            s(&gens),
                        ],
                    )
                    .await?;
                    let _ = std::fs::remove_dir_all(dir.join(format!("work-r{r}")));
                }
                (vec!["--stages".to_string(), o("stages")], a.replicas)
            }
            (None, Some(app)) => {
                let k = a.stage.expect("clap: requires");
                let zip =
                    std::fs::read(app).with_context(|| format!("reading {}", app.display()))?;
                let hash = store.put(&crucible_crypto::seal(&public, &zip)?).await?;
                (
                    vec![
                        "--stage".to_string(),
                        k.to_string(),
                        "--app-blob".into(),
                        hash,
                    ],
                    1,
                )
            }
            (None, None) => bail!("--agent or --app"),
        };
        std::fs::create_dir_all(&gens)?;
        let mut hs: Vec<(Secret, &[u8])> = vec![(Secret::PlatformKey, pk)];
        if let Some(c) = &cred {
            hs.push((Secret::DevModelCred, c));
        }
        let mut args = vec![
            "--root".into(),
            s(&sroot),
            "--taskset".into(),
            ts_s.clone(),
            "--results".into(),
            s(&gens),
            "--store".into(),
            store_spec.clone(),
            "--recipient".into(),
            run_key.public().to_string(),
            "--cred-source".into(),
            if cred.is_some() {
                "github-secret".into()
            } else {
                "none".into()
            },
            "--model".into(),
            a.model.clone(),
            "--endpoint".into(),
            a.endpoint.clone(),
            "--budget".into(),
            a.budget.clone(),
            "--eval-id".into(),
            eval_id.clone(),
            "--out".into(),
            s(&handoff),
        ];
        args.extend(mode_args.iter().cloned());
        if !drv.step(&exe, "handoff", "handoff", &hs, &args).await? {
            bail!("handoff failed");
        }
        let hexe = PathBuf::from(s(&handoff.join("bin/crucible")));
        for r in 1..=replicas {
            drv.step(
                &hexe,
                "score-tests",
                &format!("score-tests r{r}"),
                &[(Secret::RunKey, run_secret.as_bytes())],
                &[
                    "--handoff".into(),
                    s(&handoff),
                    "--replica".into(),
                    r.to_string(),
                    "--run-label".into(),
                    format!("{eval_id}-score-r{r}"),
                    "--out".into(),
                    s(&scores),
                ],
            )
            .await?;
        }
        let mut args = vec![
            "--root".into(),
            s(&sroot),
            "--mode".into(),
            if a.agent.is_some() {
                "agent".into()
            } else {
                "app".into()
            },
            "--eval-id".into(),
            eval_id.clone(),
            "--taskset".into(),
            ts_s.clone(),
            "--model".into(),
            a.model.clone(),
            "--replicas".into(),
            replicas.to_string(),
            "--results".into(),
            s(&gens),
            "--scores".into(),
            s(&scores),
            "--budget".into(),
            a.budget.clone(),
            "--store".into(),
            store_spec.clone(),
            "--timing-source".into(),
            timing_source.into(),
            "--out".into(),
            s(&publish),
        ];
        args.extend(mode_args);
        if !drv
            .step(
                &exe,
                "publish",
                "publish",
                &[(Secret::PlatformKey, pk)],
                &args,
            )
            .await?
        {
            bail!("publish failed");
        }
        Ok(())
    }
    .await;

    // Kubernetes: the results come back from the volume (never the
    // handoff, the work dirs or temporary files), then the namespace goes.
    if let Backend::K8s(k) = &drv.backend {
        if let Err(e) = k
            .fetch(&dir, &["./gen", "./scores", "./publish", "./store"])
            .await
        {
            eprintln!("warning: fetching the results: {e:#}");
        }
        k.delete().await;
    }
    // What stays: sealed bundles, scores, the manifest. Clear material
    // (the handoff's opened copies never touch disk; work dirs) goes.
    let _ = std::fs::remove_dir_all(&handoff);
    std::fs::write(
        dir.join("steps.json"),
        serde_json::to_string_pretty(&drv.records)? + "\n",
    )?;
    result?;
    let failed: Vec<&str> = drv
        .records
        .iter()
        .filter(|r| !r.ok)
        .map(|r| r.step.as_str())
        .collect();
    let m = publish.join("manifest.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&m)?)?;
    println!(
        "{}",
        serde_json::json!({
            "eval_id": eval_id,
            "total_score": v["total_score"],
            "stages": v["replicas"].as_array().into_iter().flatten().flat_map(|r| r["stages"].as_array().cloned().unwrap_or_default()).map(|s| serde_json::json!({"stage": s["stage"], "score": s["score"]["score"], "status": s["score"]["status"]})).collect::<Vec<_>>(),
            "manifest": m,
            "failed_steps": failed,
        })
    );
    if !failed.is_empty() {
        bail!("steps failed: {}", failed.join(", "));
    }
    Ok(())
}
