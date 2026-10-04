//! `crucible`: the platform's command line tool.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use crucible_core::taskset::USER_SCORERS;
use crucible_crypto::PublicKey;
use crucible_metering::{Price, Pricing};

mod agentpkg;
mod build;
mod cred;
mod keys;
mod package;
mod plan;
mod publish;
mod run;
mod score;
mod submit;
mod taskset_cmd;
mod worker;
mod zipdir;

use keys::{Store, load_identities};

#[derive(Parser)]
#[command(
    name = "crucible",
    version,
    about = "A general platform for evaluating agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the metering proxy. Reads {"api_key","endpoint"} as one JSON line on stdin.
    Meter(crucible_meter::MeterArgs),
    /// Run the CONNECT allowlist proxy for package registries.
    Egress {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 3128)]
        port: u16,
        /// Allowlist (config/egress.json).
        #[arg(long)]
        allow: PathBuf,
        /// JSONL log to append to.
        #[arg(long)]
        log: PathBuf,
    },
    /// Encrypt a file to a public key (age X25519 envelope).
    Seal {
        /// Public key, `age1...`.
        #[arg(long)]
        recipient: String,
        /// Input file, `-` for stdin.
        #[arg(long = "in", default_value = "-")]
        input: String,
        /// Output file, `-` for stdout.
        #[arg(long, default_value = "-")]
        out: String,
        /// Write base64 text instead of binary (e.g. for a GitHub secret).
        #[arg(long)]
        base64: bool,
    },
    /// Decrypt a sealed file. The key is picked by the key id in the file.
    Open {
        /// Private key file (`AGE-SECRET-KEY-1...`); repeat for old keys.
        #[arg(long)]
        identity: Vec<PathBuf>,
        /// Environment variable holding a private key.
        #[arg(long)]
        identity_env: Vec<String>,
        #[arg(long = "in", default_value = "-")]
        input: String,
        #[arg(long, default_value = "-")]
        out: String,
    },
    /// Store a file; prints its SHA-256.
    Put {
        /// `dir:<path>` or `github:<owner>/<repo>` (token from GITHUB_TOKEN).
        #[arg(long)]
        store: String,
        file: PathBuf,
    },
    /// Fetch a blob by SHA-256 (verified).
    Get {
        #[arg(long)]
        store: String,
        hash: String,
        #[arg(long, default_value = "-")]
        out: String,
    },
    /// Summarise replicas × stages: <dir>/<replica>/<stage>/{usage.jsonl,result.json,timing.json}.
    Report {
        #[arg(long)]
        run_dir: PathBuf,
        #[command(flatten)]
        price: PriceArgs,
        #[command(flatten)]
        out: OutArgs,
    },
    /// Token totals, cache hit rate and equivalent cost of one usage.jsonl.
    Price {
        #[arg(long)]
        usage: PathBuf,
        #[command(flatten)]
        price: PriceArgs,
        #[command(flatten)]
        out: OutArgs,
    },
    /// Task sets: pack a source tree into sealed blobs, validate, fetch inputs.
    Taskset {
        #[command(subcommand)]
        cmd: TasksetCmd,
    },
    /// Validate the eval.yml dispatch inputs (IN_* env vars); write job outputs.
    Plan(Box<PlanArgs>),
    /// Validate the score.yml dispatch inputs (IN_* env vars); write job outputs.
    PlanScore(Box<PlanScoreArgs>),
    /// Fetch and validate an agent package: builtin:<name> | url:<https zip> | git:<https url>@<ref> | blob:<sha256>.
    Fetch {
        #[arg(long)]
        source: String,
        #[arg(long)]
        out: PathBuf,
        /// Where builtin agents live.
        #[arg(long, default_value = "agents")]
        builtin_dir: PathBuf,
        /// Store for `blob:<sha256>` sources.
        #[arg(long)]
        store: Option<String>,
        /// Private key(s) for `blob:` sources.
        #[arg(long)]
        identity_env: Vec<String>,
    },
    /// docker build an agent package (nothing from it runs on the host).
    Build {
        #[arg(long)]
        pkg: PathBuf,
        #[arg(long, default_value = "crucible-agent:run")]
        tag: String,
        /// KEY=VALUE build arg (repeatable).
        #[arg(long = "build-arg")]
        build_arg: Vec<String>,
        /// Fail unless /agent-build.json in the image records this commit.
        #[arg(long)]
        expect_commit: Option<String>,
    },
    /// Run the agent stage by stage. Reads {"api_key","endpoint"} as one JSON line on stdin.
    Run(Box<run::RunArgs>),
    /// Package a work dir as a stage output zip.
    Package {
        /// `web-app` or `files`.
        #[arg(long, default_value = "web-app")]
        kind: String,
        #[arg(long)]
        src: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// agent.json (for app_start_cmd).
        #[arg(long)]
        agent_json: Option<PathBuf>,
    },
    /// Model credential sources.
    Cred {
        #[command(subcommand)]
        cmd: CredCmd,
    },
    /// Seal each stage's checkpoint and logs of one replica to the platform key.
    SealOutputs {
        #[arg(long)]
        replica_dir: PathBuf,
        /// config/keys.json
        #[arg(long)]
        keys: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Extra files (agent facts) sealed together as `<out>/agent.sealed`.
        #[arg(long)]
        extra: Vec<PathBuf>,
    },
    /// Store every `*.sealed` file under a directory (hash-checked).
    PutSealed {
        #[arg(long)]
        store: String,
        dir: PathBuf,
    },
    /// Build the evaluation manifest from sealed outputs, usage and job timestamps.
    Manifest(Box<ManifestArgs>),
    /// Score stage checkpoints with the taskset's scorer: <results>/<replica>/<stage>/checkpoint.sealed -> <out>/<replica>/<stage>/score.json.
    Score(Box<ScoreArgs>),
    /// On the machine with the platform key: re-seal the selected stages'
    /// tests and checkpoints to a fresh one-run key for a scoring machine
    /// that holds no secret (`crucible score --tests-dir`). Writes
    /// <out>/{taskset.json,tests/<stage>.sealed,results/<r>/<stage>/checkpoint.sealed}
    /// and the one-run key to --key-out (mode 0600).
    ScoreHandoff(Box<ScoreHandoffArgs>),
    /// AES-256 zip of every stage's output and logs, locked with the
    /// download password from the Worker credential; stored as a plain blob
    /// and recorded as `download` in the manifest. Skipped (exit 0) when the
    /// credential has no download password.
    DownloadZip(Box<DownloadZipArgs>),
    /// Submit an evaluation, like the website (token from CRUCIBLE_TOKEN).
    Submit {
        #[command(subcommand)]
        cmd: submit::SubmitCmd,
    },
    /// Show (or wait for) an evaluation's status and scores.
    Status(submit::StatusArgs),
    /// The Worker's internal endpoints (token from CRUCIBLE_WORKER_TOKEN).
    Worker {
        /// The Worker (`https://...`; only its origin is used).
        #[arg(long)]
        worker_url: String,
        #[arg(long)]
        eval_id: String,
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
}

#[derive(Subcommand)]
enum WorkerCmd {
    /// Report progress: building | running:<stage> | scoring | failed. Best effort: a failure is a warning.
    Status { status: String },
    /// POST the manifest (with its `download`) and the final status.
    Results {
        #[arg(long)]
        manifest: PathBuf,
        /// `done` or `failed`; default: `done` if any stage was scored, else `failed`.
        #[arg(long)]
        status: Option<String>,
    },
    /// Delete the credential (idempotent). Best effort.
    DeleteCred,
}

#[derive(clap::Args)]
struct ScoreArgs {
    #[arg(long)]
    taskset: PathBuf,
    /// Score the first N stages.
    #[arg(long, conflicts_with = "stage")]
    stages: Option<usize>,
    /// Score only stage K (1-based).
    #[arg(long)]
    stage: Option<usize>,
    #[arg(long)]
    results: PathBuf,
    #[arg(long)]
    out: PathBuf,
    /// Store of the tests blobs.
    #[arg(
        long,
        required_unless_present = "tests_dir",
        conflicts_with = "tests_dir"
    )]
    store: Option<String>,
    /// Tests from `crucible score-handoff` (`<dir>/<stage>.sealed`) instead of the store.
    #[arg(long)]
    tests_dir: Option<PathBuf>,
    #[arg(long)]
    identity: Vec<PathBuf>,
    /// Environment variable holding a private key; removed from the scorer's environment.
    #[arg(long)]
    identity_env: Vec<String>,
    /// Directory of scorers (`<dir>/<taskset scorer>/score.sh`).
    #[arg(long, default_value = "scorers")]
    scorers_dir: PathBuf,
}

#[derive(clap::Args)]
struct ScoreHandoffArgs {
    #[arg(long)]
    taskset: PathBuf,
    #[arg(long, conflicts_with = "stage")]
    stages: Option<usize>,
    #[arg(long)]
    stage: Option<usize>,
    #[arg(long)]
    results: PathBuf,
    #[arg(long)]
    store: String,
    #[arg(long)]
    identity_env: Vec<String>,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    key_out: PathBuf,
}

fn stage_indices(n: usize, stage: Option<usize>, stages: Option<usize>) -> Result<Vec<usize>> {
    match (stage, stages) {
        (Some(k), _) if (1..=n).contains(&k) => Ok(vec![k - 1]),
        (Some(_), _) => bail!("--stage must be 1..={n}"),
        (None, s) => Ok((0..s.unwrap_or(n).min(n)).collect()),
    }
}

#[derive(clap::Args)]
struct DownloadZipArgs {
    #[arg(long)]
    worker_url: String,
    #[arg(long)]
    eval_id: String,
    /// `<dir>/<replica>/<stage>/{checkpoint,logs}.sealed`.
    #[arg(long)]
    results: PathBuf,
    #[arg(long)]
    identity_env: Vec<String>,
    #[arg(long)]
    store: String,
    /// Manifest to record the download in (rewritten in place).
    #[arg(long)]
    manifest: PathBuf,
}

#[derive(clap::Args)]
struct PriceArgs {
    /// Price table (config/pricing.json).
    #[arg(long)]
    pricing: PathBuf,
    /// User price {"input","cached_input","output"} USD/1M; applies to every model.
    #[arg(long)]
    price_json: Option<String>,
}

#[derive(clap::Args)]
struct OutArgs {
    /// Write the summary JSON here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Print a markdown table instead of JSON.
    #[arg(long)]
    markdown: bool,
}

impl PriceArgs {
    fn load(&self) -> Result<(Pricing, Option<Price>)> {
        let raw = std::fs::read_to_string(&self.pricing)
            .with_context(|| format!("reading {}", self.pricing.display()))?;
        let pricing = Pricing::from_json(&raw)?;
        let user = match &self.price_json {
            None => None,
            Some(s) => {
                let v: serde_json::Value = serde_json::from_str(s).context("--price-json")?;
                Some(
                    crucible_metering::parse_price(&v)?
                        .ok_or_else(|| anyhow!("--price-json needs input and output"))?,
                )
            }
        };
        Ok((pricing, user))
    }
}

impl OutArgs {
    /// Same contract as the prototype: `--json` writes a file, `--markdown`
    /// prints the table, and with neither the JSON goes to stdout.
    fn emit<T: serde::Serialize>(&self, value: &T, md: impl FnOnce() -> String) -> Result<()> {
        let json = serde_json::to_string_pretty(value)? + "\n";
        if let Some(p) = &self.json {
            std::fs::write(p, &json).with_context(|| format!("writing {}", p.display()))?;
        }
        if self.markdown {
            print!("{}", md());
        } else if self.json.is_none() {
            print!("{json}");
        }
        Ok(())
    }
}

fn read_input(path: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    if path == "-" {
        std::io::stdin().read_to_end(&mut buf)?;
    } else {
        buf = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    }
    Ok(buf)
}

fn write_output(path: &str, data: &[u8]) -> Result<()> {
    if path == "-" {
        let mut out = std::io::stdout().lock();
        out.write_all(data)?;
        out.flush()?;
    } else {
        std::fs::write(path, data).with_context(|| format!("writing {path}"))?;
    }
    Ok(())
}

async fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Meter(args) => crucible_meter::run(args).await.map_err(|e| anyhow!("{e}")),
        Cmd::Egress {
            bind,
            port,
            allow,
            log,
        } => {
            let raw = std::fs::read_to_string(&allow)
                .with_context(|| format!("reading {}", allow.display()))?;
            let cfg = crucible_egress::EgressConfig::from_json(&raw, log)?;
            let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
            eprintln!("egress: listening on {}", listener.local_addr()?);
            tokio::select! {
                r = crucible_egress::serve(listener, cfg) => r?,
                _ = tokio::signal::ctrl_c() => {}
            }
            Ok(())
        }
        Cmd::Seal {
            recipient,
            input,
            out,
            base64,
        } => {
            let key: PublicKey = recipient.parse()?;
            let sealed = crucible_crypto::seal(&key, &read_input(&input)?)?;
            if base64 {
                write_output(&out, (cred::base64_encode(&sealed) + "\n").as_bytes())
            } else {
                write_output(&out, &sealed)
            }
        }
        Cmd::Open {
            identity,
            identity_env,
            input,
            out,
        } => {
            let keys = load_identities(&identity, &identity_env)?;
            let plain = crucible_crypto::open(&keys, &read_input(&input)?)?;
            write_output(&out, &plain)
        }
        Cmd::Put { store, file } => {
            let data =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            println!("{}", Store::parse(&store)?.put(&data).await?);
            Ok(())
        }
        Cmd::Get { store, hash, out } => {
            let data = Store::parse(&store)?.get(&hash).await?;
            write_output(&out, &data)
        }
        Cmd::Report {
            run_dir,
            price,
            out,
        } => {
            let (pricing, user) = price.load()?;
            let replicas = crucible_report::runs::load_run_dir(&run_dir)?;
            let report = crucible_report::runs::aggregate(&replicas, &pricing, user);
            out.emit(&report, || crucible_report::runs::markdown(&report))
        }
        Cmd::Price { usage, price, out } => {
            let (pricing, user) = price.load()?;
            let records = read_usage(&usage)?;
            let summary = crucible_report::usage::summarise(&records, &pricing, user);
            out.emit(&summary, || crucible_report::usage::markdown(&summary))
        }
        Cmd::Taskset { cmd } => taskset(cmd).await,
        Cmd::Plan(args) => {
            let PlanArgs {
                inputs,
                root,
                github_output,
            } = *args;
            plan::fetch_user_taskset(
                &root,
                &inputs.taskset,
                &inputs.owner,
                &plan::options_results_url(&inputs.options),
                &inputs.worker_url,
            )
            .await?;
            let out = plan::plan(&inputs, &root)?;
            write_plan(&out, github_output.as_deref())
        }
        Cmd::Fetch {
            source,
            out,
            builtin_dir,
            store,
            identity_env,
        } => {
            let src = agentpkg::Source::parse(&source)?;
            let blob_access = match &src {
                agentpkg::Source::Blob(_) => Some((
                    Store::parse(
                        store
                            .as_deref()
                            .ok_or_else(|| anyhow!("blob: sources need --store"))?,
                    )?,
                    load_identities(&[], &identity_env)?,
                )),
                _ => None,
            };
            let facts = agentpkg::fetch(&src, &builtin_dir, &out, blob_access.as_ref()).await?;
            println!("{}", serde_json::to_string(&facts)?);
            Ok(())
        }
        Cmd::Build {
            pkg,
            tag,
            build_arg,
            expect_commit,
        } => {
            let opts = build::BuildOpts {
                build_args: build_arg,
                expect_commit,
            };
            let facts = build::build(&pkg, &tag, &opts)?;
            println!("{}", serde_json::to_string(&facts)?);
            Ok(())
        }
        Cmd::Run(args) => run::run(*args).await,
        Cmd::Package {
            kind,
            src,
            out,
            agent_json,
        } => {
            let kind: crucible_core::taskset::OutputKind =
                serde_json::from_value(serde_json::Value::String(kind)).context("--kind")?;
            let cmd = match agent_json {
                Some(p) => {
                    let spec: crucible_core::AgentSpec =
                        serde_json::from_slice(&std::fs::read(&p)?)?;
                    spec.validate()?;
                    spec.app_start_cmd()
                }
                None => vec!["npm".into(), "start".into()],
            };
            let (zip, stats) = package::package(kind, &src, &cmd)?;
            std::fs::write(&out, &zip)?;
            eprintln!(
                "packaged {} files ({} bytes), dropped {} symlinks -> {} ({} bytes)",
                stats.files,
                stats.bytes,
                stats.dropped_links,
                out.display(),
                zip.len()
            );
            Ok(())
        }
        Cmd::Cred { cmd } => {
            match cmd {
                CredCmd::Open {
                    source,
                    sealed_env,
                    identity,
                    identity_env,
                    endpoint,
                    worker_url,
                    eval_id,
                } => {
                    let src = cred::CredSource::parse(&source)?;
                    let keys = load_identities(&identity, &identity_env)?;
                    let (sealed, expect) =
                        match src {
                            cred::CredSource::GithubSecret => {
                                (cred::read_sealed(src, &sealed_env, None).await?, None)
                            }
                            cred::CredSource::WorkersKv => {
                                let (url, id) =
                                    worker_url.as_deref().zip(eval_id.as_deref()).ok_or_else(
                                        || anyhow!("workers-kv needs --worker-url and --eval-id"),
                                    )?;
                                let w = worker::Worker::from_env(url)?;
                                (
                                    cred::read_sealed(src, &sealed_env, Some((&w, id))).await?,
                                    Some(id),
                                )
                            }
                        };
                    let line = cred::open_credential(&sealed, &keys, endpoint.as_deref(), expect)?;
                    let mut out = std::io::stdout().lock();
                    out.write_all(line.as_bytes())?;
                    out.write_all(b"\n")?;
                    out.flush()?;
                    Ok(())
                }
            }
        }
        Cmd::SealOutputs {
            replica_dir,
            keys,
            out,
            extra,
        } => {
            let key = keys::current_public_key(&keys)?;
            for (stage, what, sha) in publish::seal_outputs(&replica_dir, &key, &out, &extra)? {
                eprintln!("sealed {stage}/{what}: {sha}");
            }
            Ok(())
        }
        Cmd::Manifest(a) => manifest(*a),
        Cmd::PutSealed { store, dir } => {
            let store = Store::parse(&store)?;
            let files = publish::sealed_files(&dir)?;
            for f in &files {
                let data = std::fs::read(f)?;
                crucible_crypto::sealed_key_id(&data)
                    .with_context(|| format!("{} is not sealed", f.display()))?;
                let want = crucible_store::sha256_hex(&data);
                let got = store.put(&data).await?;
                if got != want {
                    bail!("store returned {got} for {want}");
                }
            }
            eprintln!("stored {} sealed files", files.len());
            Ok(())
        }
        Cmd::PlanScore(args) => {
            let i = &args.inputs;
            plan::fetch_user_taskset(
                &args.root,
                &i.taskset,
                &i.owner,
                &i.results_url,
                &i.worker_url,
            )
            .await?;
            let out = plan::plan_score(&args.inputs, &args.root)?;
            write_plan(&out, args.github_output.as_deref())
        }
        Cmd::Score(a) => {
            let ts = taskset_cmd::load(&a.taskset)?;
            ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
            let stages = stage_indices(ts.stages.len(), a.stage, a.stages)?;
            if !crucible_core::is_slug(&ts.scorer.name, 64) {
                bail!("bad scorer name");
            }
            let scorer = a.scorers_dir.join(&ts.scorer.name).join("score.sh");
            let keys = load_identities(&a.identity, &a.identity_env)?;
            let store = a.store.as_deref().map(Store::parse).transpose()?;
            let tests = match (&store, &a.tests_dir) {
                (_, Some(d)) => score::TestsFrom::Dir(d),
                (Some(s), None) => score::TestsFrom::Store(s),
                (None, None) => bail!("--store or --tests-dir"),
            };
            let done = score::score(&score::ScoreOpts {
                taskset: &ts,
                stages,
                results: &a.results,
                out: &a.out,
                tests,
                keys: &keys,
                scorer: &scorer,
                scrub_env: &a.identity_env,
            })
            .await?;
            eprintln!("scored {} stage checkpoints", done.len());
            Ok(())
        }
        Cmd::ScoreHandoff(a) => {
            let ts = taskset_cmd::load(&a.taskset)?;
            ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
            let stages = stage_indices(ts.stages.len(), a.stage, a.stages)?;
            let keys = load_identities(&[], &a.identity_env)?;
            let run_key = score::handoff(
                &ts,
                &stages,
                &a.results,
                &Store::parse(&a.store)?,
                &keys,
                &a.out,
            )
            .await?;
            std::fs::copy(&a.taskset, a.out.join("taskset.json"))?;
            write_private(&a.key_out, run_key.to_secret_string().as_bytes())?;
            eprintln!("handoff written for {} stage(s)", stages.len());
            Ok(())
        }
        Cmd::DownloadZip(a) => {
            let w = worker::Worker::from_env(&a.worker_url)?;
            let keys = load_identities(&[], &a.identity_env)?;
            let sealed = w.get_cred(&a.eval_id).await?;
            let Some(password) = cred::download_password(&sealed, &keys, &a.eval_id)? else {
                eprintln!("no download password: skipping the download zip");
                return Ok(());
            };
            let zip = publish::download_zip(&a.results, &keys, &password)?;
            drop(password);
            let sha256 = Store::parse(&a.store)?.put(&zip).await?;
            let mut m: crucible_core::Manifest =
                serde_json::from_slice(&std::fs::read(&a.manifest)?)?;
            m.download = Some(crucible_core::manifest::DownloadRef { sha256 });
            std::fs::write(&a.manifest, serde_json::to_string_pretty(&m)? + "\n")?;
            eprintln!("download zip stored ({} bytes)", zip.len());
            Ok(())
        }
        Cmd::Submit { cmd } => submit::submit(cmd).await,
        Cmd::Status(a) => submit::status_cmd(a).await,
        Cmd::Worker {
            worker_url,
            eval_id,
            cmd,
        } => {
            let w = worker::Worker::from_env(&worker_url)?;
            match cmd {
                WorkerCmd::Status { status } => {
                    match w.status(&eval_id, &status).await {
                        Ok(()) => eprintln!("status {status} reported"),
                        Err(e) => println!("::warning::status {status} not reported: {e:#}"),
                    }
                    Ok(())
                }
                WorkerCmd::DeleteCred => {
                    match w.delete_cred(&eval_id).await {
                        Ok(()) => eprintln!("credential deleted"),
                        Err(e) => println!("::warning::credential not deleted: {e:#}"),
                    }
                    Ok(())
                }
                WorkerCmd::Results { manifest, status } => {
                    let raw: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&manifest)?)?;
                    let m: crucible_core::Manifest = serde_json::from_value(raw.clone())?;
                    if m.eval_id != eval_id {
                        bail!("manifest eval_id does not match --eval-id");
                    }
                    let scored = m
                        .replicas
                        .iter()
                        .flat_map(|r| &r.stages)
                        .any(|s| s.score.is_some_and(|s| s.status.is_scored()));
                    let status = match status.as_deref() {
                        None => if scored { "done" } else { "failed" }.to_owned(),
                        Some(s @ ("done" | "failed")) => s.to_owned(),
                        Some(_) => bail!("--status must be done or failed"),
                    };
                    w.results(&eval_id, &raw, &status).await?;
                    eprintln!("results delivered ({status})");
                    Ok(())
                }
            }
        }
    }
}

fn write_plan(
    out: &std::collections::BTreeMap<&str, String>,
    github_output: Option<&Path>,
) -> Result<()> {
    let mut text = String::new();
    for (k, v) in out {
        if v.contains('\n') {
            bail!("output {k} contains a newline");
        }
        text.push_str(&format!("{k}={v}\n"));
    }
    match github_output {
        Some(p) => {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(p)?;
            f.write_all(text.as_bytes())?;
            eprint!("{text}");
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[derive(clap::Args)]
struct PlanArgs {
    #[command(flatten)]
    inputs: plan::PlanInputs,
    /// Repository root (agents/, tasksets/).
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Append key=value lines here (GitHub's $GITHUB_OUTPUT); else stdout.
    #[arg(long, env = "GITHUB_OUTPUT")]
    github_output: Option<PathBuf>,
}

#[derive(clap::Args)]
struct PlanScoreArgs {
    #[command(flatten)]
    inputs: plan::ScorePlanInputs,
    #[arg(long, default_value = ".")]
    root: PathBuf,
    #[arg(long, env = "GITHUB_OUTPUT")]
    github_output: Option<PathBuf>,
}

#[derive(Subcommand)]
enum TasksetCmd {
    /// Seal each stage's inputs and tests, store them, write taskset.json.
    Pack {
        /// source.json: stages, their dirs, which paths are inputs / tests.
        #[arg(long, required_unless_present = "zip", requires = "src_dir")]
        source: Option<PathBuf>,
        /// Root of the unpacked source tree.
        #[arg(long)]
        src_dir: Option<PathBuf>,
        /// A user's taskset zip (source.json + stage dirs) instead of
        /// --source/--src-dir; checked with the user rules (`validate`).
        #[arg(long, conflicts_with_all = ["source", "src_dir"], requires = "name")]
        zip: Option<PathBuf>,
        /// Register under this name (`u-<16 hex>` for a user taskset); the
        /// source's own name becomes the title.
        #[arg(long)]
        name: Option<String>,
        /// config/keys.json (the current public key seals the blobs).
        #[arg(long)]
        keys: PathBuf,
        #[arg(long)]
        store: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Check a taskset. PATH is a taskset.json (format, total time <= the
    /// platform limit), or a taskset source to upload: a .zip, a directory
    /// or its source.json (format, every stage's inputs and tests present
    /// and disjoint, total time <= 18000 s, scorer offered to users).
    Validate {
        path: PathBuf,
        #[arg(long, default_value_t = crucible_core::taskset::MAX_TOTAL_TIME_S)]
        max_total_s: u64,
        /// Source only: allow this scorer (repeatable; default: the scorers
        /// offered for uploaded tasksets, i.e. playwright).
        #[arg(long)]
        allow_scorer: Vec<String>,
    },
    /// Report a `taskset-pack` outcome to the Worker (token from
    /// CRUCIBLE_WORKER_TOKEN): the packed taskset.json, or a failure.
    Report {
        /// The Worker (`https://...`; only its origin is used).
        #[arg(long)]
        worker_url: String,
        /// `u-<16 hex>`.
        #[arg(long)]
        id: String,
        #[arg(long, conflicts_with = "error", required_unless_present = "error")]
        taskset: Option<PathBuf>,
        /// Why packing failed (shown to the uploader; at most 500 characters).
        #[arg(long)]
        error: Option<String>,
    },
    /// Download and unpack the inputs (never the tests) of the first N stages.
    Inputs {
        #[arg(long)]
        taskset: PathBuf,
        #[arg(long)]
        store: String,
        #[arg(long)]
        identity: Vec<PathBuf>,
        #[arg(long)]
        identity_env: Vec<String>,
        #[arg(long)]
        stages: Option<usize>,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum CredCmd {
    /// Print the opened credential as one JSON line (pipe it into `crucible run`).
    Open {
        /// `github-secret` (development) or `workers-kv`.
        #[arg(long)]
        source: String,
        /// Environment variable holding the sealed credential.
        #[arg(long, default_value = "DEV_MODEL_CRED")]
        sealed_env: String,
        #[arg(long)]
        identity: Vec<PathBuf>,
        #[arg(long)]
        identity_env: Vec<String>,
        /// Overrides the endpoint stored in the credential.
        #[arg(long)]
        endpoint: Option<String>,
        /// workers-kv: the Worker (token from CRUCIBLE_WORKER_TOKEN).
        #[arg(long)]
        worker_url: Option<String>,
        /// workers-kv: the evaluation; must match the one inside the credential.
        #[arg(long)]
        eval_id: Option<String>,
    },
}

#[derive(clap::Args)]
struct ManifestArgs {
    #[arg(long)]
    eval_id: String,
    #[arg(long)]
    taskset: PathBuf,
    /// `agent` (generated by an agent) or `app` (an uploaded output).
    #[arg(long, default_value = "agent")]
    mode: String,
    /// agent: the first N stages ran.
    #[arg(long)]
    stages: Option<usize>,
    /// app: the stage (1-based) the upload was scored on.
    #[arg(long)]
    stage: Option<usize>,
    #[arg(long, default_value = "")]
    model: String,
    /// `<dir>/<replica>/<stage>/score.json` from `crucible score`.
    #[arg(long)]
    scores: Option<PathBuf>,
    /// Private key(s) to read the sealed logs and agent facts.
    #[arg(long)]
    identity_env: Vec<String>,
    /// `<dir>/<replica>/<stage>/{usage.jsonl,timing.json,*.sealed}`.
    #[arg(long)]
    results: PathBuf,
    #[arg(long, default_value_t = 1)]
    replicas: u32,
    /// GitHub `GET .../actions/runs/{id}/attempts/{n}/jobs` response.
    #[arg(long)]
    jobs: Option<PathBuf>,
    #[arg(long, default_value = "Run agent stages")]
    run_step: String,
    /// `<github_id>:<login>` of the submitter.
    #[arg(long, default_value = "")]
    owner: String,
    /// Publish the manifest in clear (default: archive it sealed only).
    #[arg(long, default_value = "false")]
    score_public: String,
    #[arg(long, env = "GITHUB_REPOSITORY")]
    repository: Option<String>,
    #[arg(long, env = "GITHUB_RUN_ID")]
    run_id: Option<u64>,
    #[arg(long, env = "GITHUB_RUN_ATTEMPT")]
    run_attempt: Option<u32>,
    #[command(flatten)]
    price: PriceArgs,
    #[arg(long)]
    out: PathBuf,
}

async fn taskset(cmd: TasksetCmd) -> Result<()> {
    match cmd {
        TasksetCmd::Pack {
            source,
            src_dir,
            zip,
            name,
            keys,
            store,
            out,
        } => {
            let key = keys::current_public_key(&keys)?;
            let store = Store::parse(&store)?;
            let tmp = tempfile::tempdir()?;
            let (source, src_dir, rules) = match zip {
                Some(z) => {
                    let root = taskset_cmd::unpack_source_zip(&std::fs::read(&z)?, tmp.path())?;
                    (root.join("source.json"), root, Some(USER_SCORERS))
                }
                None => (
                    source.expect("clap: required"),
                    src_dir.expect("clap: required"),
                    None,
                ),
            };
            // Errors name files relative to the upload, not this machine.
            let mut ts = taskset_cmd::pack(&source, &src_dir, rules, &key, &store)
                .await
                .map_err(|e| {
                    anyhow!(
                        "{}",
                        format!("{e:#}").replace(&format!("{}/", src_dir.display()), "")
                    )
                })?;
            if let Some(n) = name {
                if !crucible_core::is_slug(&n, 64) {
                    bail!("--name must match [a-z0-9][a-z0-9-]{{0,63}}");
                }
                ts.title = Some(std::mem::replace(&mut ts.name, n));
            }
            std::fs::write(&out, serde_json::to_string_pretty(&ts)? + "\n")?;
            eprintln!("wrote {}", out.display());
            Ok(())
        }
        TasksetCmd::Validate {
            path,
            max_total_s,
            allow_scorer,
        } => {
            let is_zip = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
            let is_source = path.file_name().is_some_and(|n| n == "source.json");
            if !(is_zip || is_source || path.is_dir()) {
                let ts = taskset_cmd::load(&path)?;
                ts.validate(max_total_s)?;
                println!(
                    "{}: {} stages, stage time {}s, total limit {}s (platform max {max_total_s}s): ok",
                    ts.name,
                    ts.stages.len(),
                    ts.stage_time_s(),
                    ts.total_time_limit_s
                );
                return Ok(());
            }
            let tmp = tempfile::tempdir()?;
            let root = if is_zip {
                taskset_cmd::unpack_source_zip(&std::fs::read(&path)?, tmp.path())?
            } else if is_source {
                path.parent().unwrap_or(Path::new(".")).to_path_buf()
            } else {
                taskset_cmd::source_root(&path)?
            };
            let allowed: Vec<&str> = if allow_scorer.is_empty() {
                USER_SCORERS.to_vec()
            } else {
                allow_scorer.iter().map(String::as_str).collect()
            };
            // A built-in taskset keeps its stage dirs under source/.
            let src_dir =
                if !is_zip && root.join("taskset.json").is_file() && root.join("source").is_dir() {
                    root.join("source")
                } else {
                    root.clone()
                };
            let p = taskset_cmd::prepare(&root.join("source.json"), &src_dir, Some(&allowed))?;
            for (s, (_, _, ni, nt)) in p.src.stages.iter().zip(&p.zips) {
                println!(
                    "  {}: {ni} input files, {nt} test files, {}s",
                    s.id, s.time_limit_s
                );
            }
            println!(
                "{}: {} stages, scorer {}, total limit {}s (platform max {}s): ok",
                p.src.name,
                p.src.stages.len(),
                p.src.scorer.name,
                p.src.total_time_limit_s,
                crucible_core::taskset::MAX_TOTAL_TIME_S
            );
            Ok(())
        }
        TasksetCmd::Report {
            worker_url,
            id,
            taskset,
            error,
        } => {
            if !crucible_core::taskset::is_user_taskset_id(&id) {
                bail!("--id must be u-<16 hex>");
            }
            let body = match (taskset, error) {
                (Some(p), _) => {
                    let ts = taskset_cmd::load(&p)?;
                    if ts.name != id {
                        bail!("{} is named {:?}, not {id}", p.display(), ts.name);
                    }
                    serde_json::json!({"status": "ready", "taskset": ts})
                }
                (None, Some(e)) => serde_json::json!({
                    "status": "failed",
                    "error": e.chars().take(500).collect::<String>(),
                }),
                (None, None) => bail!("--taskset or --error"),
            };
            worker::Worker::from_env(&worker_url)?
                .taskset_result(&id, &body)
                .await
        }
        TasksetCmd::Inputs {
            taskset,
            store,
            identity,
            identity_env,
            stages,
            out,
        } => {
            let ts = taskset_cmd::load(&taskset)?;
            ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
            let n = stages.unwrap_or(ts.stages.len()).min(ts.stages.len());
            let keys = load_identities(&identity, &identity_env)?;
            taskset_cmd::fetch_inputs(&ts, n, &Store::parse(&store)?, &keys, &out).await
        }
    }
}

fn manifest(a: ManifestArgs) -> Result<()> {
    use crucible_core::manifest::Mode;
    let ts = taskset_cmd::load(&a.taskset)?;
    let (pricing, user) = a.price.load()?;
    let keys = load_identities(&[], &a.identity_env)?;
    let mode = match a.mode.as_str() {
        "agent" => Mode::Agent,
        "app" => Mode::App,
        _ => bail!("--mode must be agent or app"),
    };
    let (stages_run, agent) = match mode {
        Mode::Agent => (
            a.stages.unwrap_or(ts.stages.len()).min(ts.stages.len()),
            agent_ref(&a, &keys)?,
        ),
        Mode::App => (
            a.stage
                .filter(|k| (1..=ts.stages.len()).contains(k))
                .ok_or_else(|| anyhow!("app mode needs --stage 1..={}", ts.stages.len()))?,
            crucible_core::manifest::AgentRef {
                name: "uploaded-output".into(),
                version: "-".into(),
                package: None,
                commit: None,
            },
        ),
    };
    let jobs = match &a.jobs {
        Some(p) => Some(std::fs::read_to_string(p)?),
        None => None,
    };
    let run = match (&a.repository, a.run_id) {
        (Some(repo), Some(id)) => Some(crucible_core::manifest::RunRef {
            repository: repo.clone(),
            run_id: id,
            run_attempt: a.run_attempt.unwrap_or(1),
        }),
        _ => None,
    };
    let m = publish::build_manifest(&publish::ManifestInputs {
        eval_id: &a.eval_id,
        created_at: humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string(),
        taskset: &ts,
        mode,
        stages_run,
        model: &a.model,
        agent,
        results: &a.results,
        replicas: a.replicas,
        jobs_json: jobs.as_deref(),
        run_step: &a.run_step,
        run,
        pricing: &pricing,
        user_price: user,
        owner: match a.owner.trim() {
            "" => None,
            o if plan::owner_ok(o) => {
                let (id, login) = o.split_once(':').expect("checked");
                Some(crucible_core::manifest::Owner {
                    github_id: id.parse()?,
                    login: login.into(),
                })
            }
            _ => bail!("--owner must be <github_id>:<login>"),
        },
        score_public: a.score_public.trim() == "true",
        keys: &keys,
        scores: a.scores.as_deref(),
    })?;
    std::fs::write(&a.out, serde_json::to_string_pretty(&m)? + "\n")?;
    Ok(())
}

/// The agent of an agent-mode run, from the sealed facts of the first
/// replica that left them.
fn agent_ref(
    a: &ManifestArgs,
    keys: &[crucible_crypto::PrivateKey],
) -> Result<crucible_core::manifest::AgentRef> {
    // Agent facts: from the first replica that sealed them.
    let mut facts = serde_json::Value::Null;
    let mut build: Option<serde_json::Value> = None;
    for r in 1..=a.replicas {
        let p = a.results.join(r.to_string()).join("agent.sealed");
        if p.is_file() {
            for (name, data) in publish::read_sealed_zip(&p, keys)? {
                match name.as_str() {
                    "facts.json" => facts = serde_json::from_slice(&data)?,
                    "build.json" => build = serde_json::from_slice(&data).ok(),
                    _ => {}
                }
            }
            break;
        }
    }
    if facts.is_null() {
        bail!("no replica left agent facts (agent.sealed)");
    }
    let text = |v: &serde_json::Value| v.as_str().map(|s| s.chars().take(100).collect::<String>());
    // Builtin agents: the source commit the build was pinned to (checked
    // against the image's /agent-build.json by `crucible build`).
    let commit = text(&facts["commit"]).or_else(|| {
        build
            .as_ref()
            .and_then(|b| text(&b["expected_commit"]).or_else(|| text(&b["agent_build"]["commit"])))
    });
    let package = match (
        text(&facts["package_sha256"]),
        text(&facts["package_key_id"]),
    ) {
        (Some(sha256), Some(key_id)) => Some(crucible_core::BlobRef { sha256, key_id }),
        _ => None,
    };
    Ok(crucible_core::manifest::AgentRef {
        name: text(&facts["agent"]["name"]).ok_or_else(|| anyhow!("facts: agent.name missing"))?,
        version: text(&facts["agent"]["version"]).unwrap_or_else(|| "0".into()),
        package,
        commit,
    })
}

/// Create `path` readable by its owner only, then write `data`.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
        .with_context(|| format!("creating {}", path.display()))?
        .write_all(data)?;
    Ok(())
}

/// A missing log means no requests were made (prototype behaviour).
fn read_usage(path: &Path) -> Result<Vec<crucible_core::UsageRecord>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(crucible_report::usage::parse_jsonl(&s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.cmd).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("crucible: {e:#}");
            ExitCode::from(2)
        }
    }
}
