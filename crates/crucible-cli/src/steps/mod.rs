//! The step layer (docs/executors.md §2.1): one evaluation is a few steps,
//! each one command `crucible step <name>`, independent of who schedules it
//! (GitHub Actions, `crucible eval local`, later Nomad or Kubernetes).
//!
//! Every step declares, statically, what it needs: input bundles, the
//! output bundle, the secrets it may hold, and its capabilities (containers,
//! sandbox network, internet, which machine pool). A scheduler reads
//! [`STEPS`] (`crucible step list`) and delivers exactly the listed
//! secrets. The step itself enforces the list: it reads only the secrets of
//! its own spec, refuses a secrets directory holding any other, and removes
//! every secret variable from its environment before anything else runs,
//! so no child process (docker, plugins, git) inherits one.
//!
//! Secrets come from files (`--secrets-dir <dir>`, one file per secret,
//! named as [`Secret::name`]) or, when no directory is given, from the
//! GitHub workflow's environment variables ([`Secret::env`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use crucible_crypto::PrivateKey;
use serde::Serialize;

use crate::keys::Store;

pub mod generate;
pub mod handoff;
pub mod pack;
pub mod plugin;
pub mod publish;
pub mod score_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Secret {
    /// The platform private key (opens taskset blobs, agent packages,
    /// sealed outputs, model credentials).
    PlatformKey,
    /// The Worker's token (status, credentials, results).
    WorkerToken,
    /// Read access to the blob store (`github:`: GITHUB_TOKEN).
    StoreRead,
    /// Write access to the blob store.
    StoreWrite,
    /// The repository token: data branch commits, the run's job
    /// timestamps (GitHub only).
    RepoToken,
    /// The private half of the one-run key of a handoff.
    RunKey,
    /// The development model credential, sealed to the platform key.
    DevModelCred,
}

impl Secret {
    pub const ALL: [Secret; 7] = [
        Secret::PlatformKey,
        Secret::WorkerToken,
        Secret::StoreRead,
        Secret::StoreWrite,
        Secret::RepoToken,
        Secret::RunKey,
        Secret::DevModelCred,
    ];

    /// File name in a secrets directory.
    pub fn name(self) -> &'static str {
        match self {
            Secret::PlatformKey => "platform-key",
            Secret::WorkerToken => "worker-token",
            Secret::StoreRead => "store-read",
            Secret::StoreWrite => "store-write",
            Secret::RepoToken => "repo-token",
            Secret::RunKey => "run-key",
            Secret::DevModelCred => "dev-model-cred",
        }
    }

    /// The environment variable a GitHub workflow passes it in.
    pub fn env(self) -> &'static str {
        match self {
            Secret::PlatformKey => "CRUCIBLE_AGE_KEY",
            Secret::WorkerToken => "CRUCIBLE_WORKER_TOKEN",
            Secret::StoreRead | Secret::StoreWrite | Secret::RepoToken => "GITHUB_TOKEN",
            Secret::RunKey => "RUN_KEY",
            Secret::DevModelCred => "DEV_MODEL_CRED",
        }
    }
}

/// Every variable that may hold a secret; removed from a step's
/// environment whatever its spec says.
pub const SECRET_ENVS: &[&str] = &[
    "CRUCIBLE_AGE_KEY",
    "CRUCIBLE_WORKER_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "RUN_KEY",
    "DEV_MODEL_CRED",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Pool {
    /// Runs no untrusted code.
    Trusted,
    /// Runs untrusted code (agents, uploaded tests) in containers.
    Sandbox,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Needs {
    /// Starts containers (an execution backend).
    pub containers: bool,
    /// A network whose containers reach only the meter / egress proxy.
    pub sandbox_net: bool,
    /// The platform's own processes need the internet (meter upstream,
    /// egress proxy, image pulls, the store).
    pub internet: bool,
    pub pool: Pool,
}

#[derive(Debug, Serialize)]
pub struct StepSpec {
    pub name: &'static str,
    /// Input bundles (directories holding only public or sealed files).
    pub inputs: &'static [&'static str],
    /// Output bundle; `{replica}` is the replica number.
    pub output: &'static str,
    /// The only secrets this step may hold.
    pub secrets: &'static [Secret],
    pub needs: Needs,
    /// Whether the whole step may be retried (it changes no outside state).
    pub retry: bool,
}

const TRUSTED: Needs = Needs {
    containers: false,
    sandbox_net: false,
    internet: true,
    pool: Pool::Trusted,
};

const SANDBOX: Needs = Needs {
    containers: true,
    sandbox_net: true,
    internet: true,
    pool: Pool::Sandbox,
};

pub const STEPS: &[StepSpec] = &[
    StepSpec {
        name: "pack",
        inputs: &[],
        output: "taskset",
        secrets: &[
            Secret::PlatformKey,
            Secret::WorkerToken,
            Secret::StoreRead,
            Secret::StoreWrite,
        ],
        needs: TRUSTED,
        retry: true,
    },
    StepSpec {
        name: "plugin-open",
        inputs: &[],
        output: "plugin-build",
        secrets: &[Secret::PlatformKey, Secret::WorkerToken, Secret::StoreRead],
        needs: TRUSTED,
        retry: true,
    },
    StepSpec {
        name: "plugin-build",
        inputs: &["plugin-build"],
        output: "plugin-outcome",
        secrets: &[Secret::RunKey],
        needs: Needs {
            containers: true,
            sandbox_net: false,
            internet: true,
            pool: Pool::Sandbox,
        },
        retry: true,
    },
    StepSpec {
        name: "plugin-report",
        inputs: &["plugin-outcome"],
        output: "none",
        secrets: &[Secret::WorkerToken],
        needs: TRUSTED,
        retry: true,
    },
    StepSpec {
        name: "plan",
        inputs: &[],
        output: "plan",
        secrets: &[Secret::WorkerToken],
        needs: TRUSTED,
        retry: true,
    },
    StepSpec {
        name: "plan-score",
        inputs: &[],
        output: "plan",
        secrets: &[Secret::WorkerToken],
        needs: TRUSTED,
        retry: true,
    },
    StepSpec {
        name: "generate",
        inputs: &["plan"],
        output: "gen-r{replica}",
        secrets: &[
            Secret::PlatformKey,
            Secret::WorkerToken,
            Secret::StoreRead,
            Secret::DevModelCred,
        ],
        needs: SANDBOX,
        retry: false,
    },
    StepSpec {
        name: "handoff",
        inputs: &["plan", "gen-r*"],
        output: "handoff",
        secrets: &[
            Secret::PlatformKey,
            Secret::WorkerToken,
            Secret::StoreRead,
            Secret::DevModelCred,
        ],
        needs: TRUSTED,
        retry: true,
    },
    StepSpec {
        name: "score-tests",
        inputs: &["handoff"],
        output: "scores-r{replica}",
        secrets: &[Secret::RunKey],
        needs: SANDBOX,
        retry: false,
    },
    StepSpec {
        name: "publish",
        inputs: &["plan", "gen-r*", "scores-r*"],
        output: "manifest",
        secrets: &[
            Secret::PlatformKey,
            Secret::WorkerToken,
            Secret::StoreRead,
            Secret::StoreWrite,
            Secret::RepoToken,
        ],
        needs: TRUSTED,
        retry: false,
    },
];

pub fn spec(name: &str) -> &'static StepSpec {
    STEPS
        .iter()
        .find(|s| s.name == name)
        .expect("every step command has a spec")
}

/// Remove every secret variable from this process's environment.
///
/// # Safety
/// Changes the process environment: call only while no other thread
/// exists (`main` does, before starting the runtime).
pub unsafe fn scrub_env() {
    for k in SECRET_ENVS {
        // SAFETY: the caller guarantees a single thread.
        unsafe { std::env::remove_var(k) };
    }
}

/// The secrets a step holds: only those of its spec.
pub struct Secrets {
    step: &'static StepSpec,
    vals: BTreeMap<Secret, Vec<u8>>,
}

impl Secrets {
    /// Read the step's secrets (from `dir`, else from the environment).
    /// `main` then calls [`scrub_env`].
    pub fn load(step: &'static StepSpec, dir: Option<&Path>) -> Result<Secrets> {
        let mut vals = BTreeMap::new();
        match dir {
            Some(d) => {
                for e in std::fs::read_dir(d).with_context(|| format!("reading {}", d.display()))? {
                    let name = e?.file_name().to_string_lossy().into_owned();
                    let Some(s) = Secret::ALL.iter().find(|s| s.name() == name) else {
                        bail!("secrets dir: unknown file {name}");
                    };
                    if !step.secrets.contains(s) {
                        bail!("step {} may not hold {name}", step.name);
                    }
                }
                for &s in step.secrets {
                    let p = d.join(s.name());
                    if p.is_file() {
                        vals.insert(s, std::fs::read(&p)?);
                    }
                }
            }
            None => {
                for &s in step.secrets {
                    if let Some(v) = std::env::var_os(s.env()).filter(|v| !v.is_empty()) {
                        vals.insert(s, v.into_encoded_bytes());
                    }
                }
            }
        }
        Ok(Secrets { step, vals })
    }

    /// No secrets at all (tests, and steps run without any).
    #[cfg(test)]
    pub fn none(step: &'static StepSpec) -> Secrets {
        Secrets {
            step,
            vals: BTreeMap::new(),
        }
    }

    fn get(&self, s: Secret) -> Result<&[u8]> {
        if !self.step.secrets.contains(&s) {
            bail!("step {} may not hold {}", self.step.name, s.name());
        }
        self.vals
            .get(&s)
            .map(Vec::as_slice)
            .ok_or_else(|| anyhow!("step {}: {} was not given", self.step.name, s.name()))
    }

    #[cfg(test)]
    pub fn has(&self, s: Secret) -> bool {
        self.vals.contains_key(&s)
    }

    pub fn bytes(&self, s: Secret) -> Result<&[u8]> {
        self.get(s)
    }

    pub fn text(&self, s: Secret) -> Result<String> {
        let v = std::str::from_utf8(self.get(s)?)
            .map_err(|_| anyhow!("{} is not text", s.name()))?
            .trim()
            .to_owned();
        if v.is_empty() {
            bail!("{} is empty", s.name());
        }
        Ok(v)
    }

    /// A private key secret (`PlatformKey` or `RunKey`).
    pub fn keys(&self, s: Secret) -> Result<Vec<PrivateKey>> {
        let raw = self.text(s)?;
        Ok(vec![PrivateKey::parse(&raw).map_err(|_| {
            anyhow!("{} is not an age identity", s.name())
        })?])
    }

    pub fn worker(&self, url: &str) -> Result<crate::worker::Worker> {
        crate::worker::Worker::new(url, self.text(Secret::WorkerToken)?)
    }

    /// The blob store; `github:` gets the read or write token.
    pub fn store(&self, spec: &str, write: bool) -> Result<Store> {
        if !spec.starts_with("github:") {
            return Store::parse(spec);
        }
        let token = if write {
            self.text(Secret::StoreWrite)?
        } else {
            self.text(Secret::StoreRead)
                .or_else(|_| self.text(Secret::StoreWrite))?
        };
        Store::with_token(spec, Some(&token))
    }
}

#[derive(clap::Args, Debug, Clone, Default)]
pub struct Common {
    /// Directory of secret files (one per secret, named as in `crucible
    /// step list`); default: the GitHub workflow's environment variables.
    #[arg(long)]
    pub secrets_dir: Option<PathBuf>,
}

#[derive(clap::Subcommand)]
pub enum StepCmd {
    /// Print every step's declaration (inputs, output, secrets, needs) as JSON.
    List,
    /// Register an uploaded taskset (taskset-pack.yml).
    Pack(Box<pack::Args>),
    /// Uploaded plugin, 1/3: open and check the package, re-seal it for the build.
    PluginOpen(Box<plugin::OpenArgs>),
    /// Uploaded plugin, 2/3: build its image and self-test it; holds only the one-run key.
    PluginBuild(Box<plugin::BuildArgs>),
    /// Uploaded plugin, 3/3: deliver the outcome to the Worker.
    PluginReport(Box<plugin::ReportArgs>),
    /// Validate the eval.yml inputs (IN_* env); write job outputs.
    Plan {
        #[command(flatten)]
        args: crate::PlanArgs,
        #[command(flatten)]
        common: Common,
    },
    /// Validate the score.yml inputs (IN_* env); write job outputs.
    PlanScore {
        #[command(flatten)]
        args: crate::PlanScoreArgs,
        #[command(flatten)]
        common: Common,
    },
    /// One replica: build the agent, run the stages in the sandbox, seal the outputs.
    Generate(Box<generate::Args>),
    /// Re-seal what scoring needs to a one-run key.
    Handoff(Box<handoff::Args>),
    /// Run the scorers (and interactive runners) on a handoff; holds only the one-run key.
    ScoreTests(Box<score_tests::Args>),
    /// Store the sealed outputs, build and archive the manifest, deliver it.
    Publish(Box<publish::Args>),
}

impl StepCmd {
    /// The spec and secrets directory of this command (`None`: `list`).
    pub fn spec(&self) -> Option<(&'static StepSpec, Option<&Path>)> {
        let (name, c) = match self {
            StepCmd::List => return None,
            StepCmd::Pack(a) => ("pack", &a.common),
            StepCmd::PluginOpen(a) => ("plugin-open", &a.common),
            StepCmd::PluginBuild(a) => ("plugin-build", &a.common),
            StepCmd::PluginReport(a) => ("plugin-report", &a.common),
            StepCmd::Plan { common, .. } => ("plan", common),
            StepCmd::PlanScore { common, .. } => ("plan-score", common),
            StepCmd::Generate(a) => ("generate", &a.common),
            StepCmd::Handoff(a) => ("handoff", &a.common),
            StepCmd::ScoreTests(a) => ("score-tests", &a.common),
            StepCmd::Publish(a) => ("publish", &a.common),
        };
        Some((spec(name), c.secrets_dir.as_deref()))
    }
}

pub async fn run(cmd: StepCmd, secrets: Option<Secrets>) -> Result<()> {
    let secrets = match (&cmd, secrets) {
        (StepCmd::List, _) => {
            println!("{}", serde_json::to_string_pretty(STEPS)?);
            return Ok(());
        }
        (_, Some(s)) => s,
        (_, None) => bail!("step secrets were not loaded"),
    };
    match cmd {
        StepCmd::List => unreachable!(),
        StepCmd::Pack(a) => pack::run(a, &secrets).await,
        StepCmd::PluginOpen(a) => plugin::open(a, &secrets).await,
        StepCmd::PluginBuild(a) => plugin::build(*a, &secrets).await,
        StepCmd::PluginReport(a) => plugin::report(*a, &secrets).await,
        StepCmd::Plan { args, .. } => {
            let crate::PlanArgs {
                inputs,
                root,
                github_output,
            } = args;
            let token = secrets.text(Secret::WorkerToken).ok();
            crate::plan::fetch_user_taskset(
                &root,
                &inputs.taskset,
                &inputs.owner,
                &crate::plan::options_results_url(&inputs.options),
                &inputs.worker_url,
                token,
            )
            .await?;
            let out = crate::plan::plan(&inputs, &root)?;
            crate::write_plan(&out, github_output.as_deref())
        }
        StepCmd::PlanScore { args: a, .. } => {
            let i = &a.inputs;
            let token = secrets.text(Secret::WorkerToken).ok();
            crate::plan::fetch_user_taskset(
                &a.root,
                &i.taskset,
                &i.owner,
                &i.results_url,
                &i.worker_url,
                token,
            )
            .await?;
            let out = crate::plan::plan_score(&a.inputs, &a.root)?;
            crate::write_plan(&out, a.github_output.as_deref())
        }
        StepCmd::Generate(a) => generate::run(*a, &secrets).await,
        StepCmd::Handoff(a) => handoff::run(*a, &secrets).await,
        StepCmd::ScoreTests(a) => score_tests::run(*a, &secrets).await,
        StepCmd::Publish(a) => publish::run(*a, &secrets).await,
    }
}

/// Refuse to run a step on a backend that cannot give it what it needs
/// (never run with weaker isolation).
pub fn check_caps(step: &StepSpec, caps: crate::executor::Caps) -> Result<()> {
    if step.needs.sandbox_net && !caps.sandbox_net {
        bail!("step {}: this backend has no sandbox network", step.name);
    }
    if step.needs.containers && !caps.pids_limit {
        bail!("step {}: this backend cannot limit processes", step.name);
    }
    Ok(())
}

/// Fail if any file under `dir` is not a sealed envelope; with `delete`,
/// remove those files instead (only sealed files leave a sandbox step).
pub fn sealed_only(dir: &Path, delete: bool) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for e in walk(dir)? {
        let ok = std::fs::read(&e)
            .ok()
            .is_some_and(|d| crucible_crypto::sealed_key_id(&d).is_ok());
        if !ok {
            if delete {
                std::fs::remove_file(&e)?;
            } else {
                bail!("{} is not sealed", e.display());
            }
        }
    }
    Ok(())
}

/// Every regular file under `dir` (symlinks are not followed).
pub fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let e = e?;
            let t = e.file_type()?;
            if t.is_dir() {
                todo.push(e.path());
            } else if t.is_file() {
                out.push(e.path());
            }
        }
    }
    Ok(out)
}

/// Test material (Playwright specs) under `dir`: never on a generation machine.
pub fn has_test_material(dir: &Path) -> Result<bool> {
    Ok(walk(dir)?.iter().any(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".spec.ts") || n == "e2e.ts")
    }))
}

/// Append to the GitHub step summary, when there is one.
pub fn step_summary(text: &str) {
    if let Some(p) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(p) {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_tests_holds_only_the_run_key() {
        assert_eq!(spec("score-tests").secrets, &[Secret::RunKey]);
        let s = Secrets::none(spec("score-tests"));
        for x in Secret::ALL {
            if x != Secret::RunKey {
                assert!(
                    s.get(x).unwrap_err().to_string().contains("may not hold"),
                    "{x:?}"
                );
            }
        }
    }

    #[test]
    fn steps_without_containers_are_trusted() {
        for s in STEPS {
            assert_eq!(
                s.needs.containers,
                s.needs.pool == Pool::Sandbox,
                "{}",
                s.name
            );
            assert!(s.needs.containers || !s.needs.sandbox_net, "{}", s.name);
        }
        // Only these hold the platform key.
        let holders: Vec<&str> = STEPS
            .iter()
            .filter(|s| s.secrets.contains(&Secret::PlatformKey))
            .map(|s| s.name)
            .collect();
        assert_eq!(
            holders,
            ["pack", "plugin-open", "generate", "handoff", "publish"]
        );
        // An uploaded plugin is built where no platform secret is.
        assert_eq!(spec("plugin-build").secrets, &[Secret::RunKey]);
    }

    #[test]
    fn secrets_dir_is_checked_against_the_spec() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("run-key"), "x").unwrap();
        let s = Secrets::load(spec("score-tests"), Some(d.path())).unwrap();
        assert!(s.has(Secret::RunKey));
        std::fs::write(d.path().join("platform-key"), "x").unwrap();
        let e = Secrets::load(spec("score-tests"), Some(d.path()))
            .err()
            .unwrap();
        assert!(e.to_string().contains("may not hold"), "{e}");
        std::fs::remove_file(d.path().join("platform-key")).unwrap();
        std::fs::write(d.path().join("other"), "x").unwrap();
        assert!(Secrets::load(spec("score-tests"), Some(d.path())).is_err());
    }

    #[test]
    fn sealed_only_deletes_clear_files() {
        let d = tempfile::tempdir().unwrap();
        let k = PrivateKey::generate();
        std::fs::create_dir_all(d.path().join("1/s")).unwrap();
        std::fs::write(
            d.path().join("1/s/checkpoint.sealed"),
            crucible_crypto::seal(&k.public(), b"x").unwrap(),
        )
        .unwrap();
        std::fs::write(d.path().join("1/s/usage.jsonl"), "{}").unwrap();
        assert!(sealed_only(d.path(), false).is_err());
        sealed_only(d.path(), true).unwrap();
        assert_eq!(walk(d.path()).unwrap().len(), 1);
        sealed_only(d.path(), false).unwrap();
    }
}
