//! `crucible`: the platform's command line tool.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use crucible_crypto::{PrivateKey, PublicKey};
use crucible_metering::{Price, Pricing};
use crucible_store::{BlobStore, GithubReleaseStore, LocalDirStore};

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
    },
    /// Decrypt a sealed file. The key is picked by the key id in the file.
    Open {
        /// Private key file (`AGE-SECRET-KEY-1...`); repeat for old keys.
        #[arg(long, required = true)]
        identity: Vec<PathBuf>,
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
    /// TODO (step 2): validate inputs and plan an evaluation.
    Plan,
    /// TODO (step 2): fetch and validate an agent package.
    Fetch,
    /// TODO (step 2): build the agent image.
    Build,
    /// TODO (step 2): run the agent stage by stage.
    Run,
    /// TODO (step 2): package outputs and logs.
    Package,
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

enum Store {
    Dir(LocalDirStore),
    Github(GithubReleaseStore),
}

impl Store {
    fn parse(spec: &str) -> Result<Store> {
        match spec.split_once(':') {
            Some(("dir", p)) if !p.is_empty() => Ok(Store::Dir(LocalDirStore::new(p))),
            Some(("github", repo)) => Ok(Store::Github(GithubReleaseStore::from_env(repo)?)),
            _ => bail!("--store must be dir:<path> or github:<owner>/<repo>"),
        }
    }

    async fn put(&self, data: &[u8]) -> Result<String> {
        Ok(match self {
            Store::Dir(s) => s.put(data).await?,
            Store::Github(s) => s.put(data).await?,
        })
    }

    async fn get(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(match self {
            Store::Dir(s) => s.get(hash).await?,
            Store::Github(s) => s.get(hash).await?,
        })
    }
}

fn load_identities(paths: &[PathBuf]) -> Result<Vec<PrivateKey>> {
    paths
        .iter()
        .map(|p| {
            let raw =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            Ok(PrivateKey::parse(&raw)?)
        })
        .collect()
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
        } => {
            let key: PublicKey = recipient.parse()?;
            let sealed = crucible_crypto::seal(&key, &read_input(&input)?)?;
            write_output(&out, &sealed)
        }
        Cmd::Open {
            identity,
            input,
            out,
        } => {
            let keys = load_identities(&identity)?;
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
        Cmd::Plan | Cmd::Fetch | Cmd::Build | Cmd::Run | Cmd::Package | Cmd::Score => {
            bail!("not implemented yet (see docs/DESIGN.md section 10)")
        }
    }
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
