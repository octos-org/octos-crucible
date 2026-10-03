//! `crucible`: the platform's command line tool.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
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
mod taskset_cmd;
mod zipdir;

use keys::{Store, load_identities};

#[derive(Parser)]
#[command(
    name = "crucible",
    version,
    about = "Evaluation infrastructure for coding agents"
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
    /// TODO (step 3): score stage checkpoints.
    Score,
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
            let out = plan::plan(&inputs, &root)?;
            let mut text = String::new();
            for (k, v) in &out {
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
                        .open(&p)?;
                    f.write_all(text.as_bytes())?;
                    eprint!("{text}");
                }
                None => print!("{text}"),
            }
            Ok(())
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
        Cmd::Cred { cmd } => match cmd {
            CredCmd::Open {
                source,
                sealed_env,
                identity,
                identity_env,
                endpoint,
            } => {
                let src = cred::CredSource::parse(&source)?;
                let sealed = cred::read_sealed(src, &sealed_env)?;
                let keys = load_identities(&identity, &identity_env)?;
                let line = cred::open_credential(&sealed, &keys, endpoint.as_deref())?;
                let mut out = std::io::stdout().lock();
                out.write_all(line.as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()?;
                Ok(())
            }
        },
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
        Cmd::Score => bail!("not implemented yet (step 3, see docs/DESIGN.md section 10)"),
    }
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

#[derive(Subcommand)]
enum TasksetCmd {
    /// Seal each stage's inputs and tests, store them, write taskset.json.
    Pack {
        /// source.json: stages, their dirs, which paths are inputs / tests.
        #[arg(long)]
        source: PathBuf,
        /// Root of the unpacked source tree.
        #[arg(long)]
        src_dir: PathBuf,
        /// config/keys.json (the current public key seals the blobs).
        #[arg(long)]
        keys: PathBuf,
        #[arg(long)]
        store: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Check a taskset.json (total time <= the platform limit).
    Validate {
        file: PathBuf,
        #[arg(long, default_value_t = crucible_core::taskset::MAX_TOTAL_TIME_S)]
        max_total_s: u64,
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
        /// `github-secret` (development) or `workers-kv` (step 4).
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
    },
}

#[derive(clap::Args)]
struct ManifestArgs {
    #[arg(long)]
    eval_id: String,
    #[arg(long)]
    taskset: PathBuf,
    #[arg(long)]
    stages: Option<usize>,
    #[arg(long)]
    model: String,
    /// Private key(s) to read the sealed logs and agent facts.
    #[arg(long)]
    identity_env: Vec<String>,
    /// `<dir>/<replica>/<stage>/{usage.jsonl,timing.json,*.sealed}`.
    #[arg(long)]
    results: PathBuf,
    #[arg(long)]
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
            keys,
            store,
            out,
        } => {
            let key = keys::current_public_key(&keys)?;
            let ts = taskset_cmd::pack(&source, &src_dir, &key, &Store::parse(&store)?).await?;
            std::fs::write(&out, serde_json::to_string_pretty(&ts)? + "\n")?;
            eprintln!("wrote {}", out.display());
            Ok(())
        }
        TasksetCmd::Validate { file, max_total_s } => {
            let ts = taskset_cmd::load(&file)?;
            ts.validate(max_total_s)?;
            println!(
                "{}: {} stages, stage time {}s, total limit {}s (platform max {max_total_s}s): ok",
                ts.name,
                ts.stages.len(),
                ts.stage_time_s(),
                ts.total_time_limit_s
            );
            Ok(())
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
    let ts = taskset_cmd::load(&a.taskset)?;
    let (pricing, user) = a.price.load()?;
    let keys = load_identities(&[], &a.identity_env)?;
    // Agent facts: from the first replica that sealed them.
    let mut facts = serde_json::Value::Null;
    let mut build: Option<serde_json::Value> = None;
    for r in 1..=a.replicas {
        let p = a.results.join(r.to_string()).join("agent.sealed");
        if p.is_file() {
            for (name, data) in publish::read_sealed_zip(&p, &keys)? {
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
    let agent = crucible_core::manifest::AgentRef {
        name: text(&facts["agent"]["name"]).ok_or_else(|| anyhow!("facts: agent.name missing"))?,
        version: text(&facts["agent"]["version"]).unwrap_or_else(|| "0".into()),
        package,
        commit,
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
        stages_run: a.stages.unwrap_or(ts.stages.len()).min(ts.stages.len()),
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
    })?;
    std::fs::write(&a.out, serde_json::to_string_pretty(&m)? + "\n")?;
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
