//! `crucible step handoff`: on the machine with the platform key, re-seal
//! what scoring needs (each selected stage's tests, every replica's
//! checkpoint, and the submitter's model credential when a scoring slot
//! uses a model) to a one-run key, and add what the scoring step runs:
//! this crucible, the plugin directories, the price table. The output
//! bundle holds only public or sealed files.
//!
//! The one-run key: `--recipient <age1...>` (the scheduler generated the
//! pair and gives the private half to `score-tests` only), or `--key-out
//! <file>` (generated here, for GitHub's masked job output).

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use crucible_crypto::{PrivateKey, PublicKey};

use super::{Common, Secret, Secrets};
use crate::cred::{self, CredSource};

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: Common,
    /// Holds scorers/, runners/, tools/ and config/pricing.json.
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long)]
    pub taskset: PathBuf,
    /// Agent mode: the first N stages.
    #[arg(long, conflicts_with = "stage")]
    pub stages: Option<usize>,
    /// App mode: the one stage (1-based) the upload is scored on.
    #[arg(long)]
    pub stage: Option<usize>,
    /// `<dir>/<replica>/<stage>/checkpoint.sealed` (gen-r* bundles).
    #[arg(long)]
    pub results: PathBuf,
    /// App mode: the sealed upload's blob (fetched into `--results` as
    /// replica 1's checkpoint of `--stage`).
    #[arg(long, requires = "stage")]
    pub app_blob: Option<String>,
    #[arg(long)]
    pub store: String,
    /// Public half of the one-run key.
    #[arg(long, required_unless_present = "key_out", conflicts_with = "key_out")]
    pub recipient: Option<String>,
    /// Generate the one-run key here and write its private half to this file (0600).
    #[arg(long)]
    pub key_out: Option<PathBuf>,
    /// `none`, `github-secret` or `workers-kv`: the submitter's model
    /// credential, handed off only if a scoring slot uses a model.
    #[arg(long, default_value = "none")]
    pub cred_source: String,
    #[arg(long, default_value = "")]
    pub model: String,
    #[arg(long, default_value = "")]
    pub endpoint: String,
    #[arg(long, default_value = "")]
    pub budget: String,
    #[arg(long, default_value = "")]
    pub eval_id: String,
    #[arg(long, default_value = "")]
    pub worker_url: String,
    #[arg(long)]
    pub out: PathBuf,
}

pub async fn run(a: Args, s: &Secrets) -> Result<()> {
    let ts = crate::taskset_cmd::load(&a.taskset)?;
    ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
    let stages = crate::stage_indices(ts.stages.len(), a.stage, a.stages)?;
    let keys = s.keys(Secret::PlatformKey)?;
    let store = s.store(&a.store, false)?;
    if let Some(h) = &a.app_blob {
        // Same layout as an agent run: <results>/1/<stage>/checkpoint.sealed.
        let k = a.stage.expect("clap: requires");
        let d = a.results.join("1").join(&ts.stages[k - 1].id);
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("checkpoint.sealed"), store.get(h).await?)?;
    }
    std::fs::create_dir_all(&a.results)?;
    let (to, run_key): (PublicKey, Option<PrivateKey>) = match &a.recipient {
        Some(r) => (r.parse()?, None),
        None => {
            let k = PrivateKey::generate();
            (k.public(), Some(k))
        }
    };
    let (mi, ms) = ts.model_use();
    let wants_model = mi != crucible_core::taskset::ModelUse::None
        || ms != crucible_core::taskset::ModelUse::None;
    let cred = match (a.cred_source.as_str(), wants_model) {
        ("none", _) | (_, false) => None,
        (src, true) => {
            let src = CredSource::parse(src)?;
            let (sealed, expect) = match src {
                CredSource::GithubSecret => (s.bytes(Secret::DevModelCred)?.to_vec(), None),
                CredSource::WorkersKv => (
                    s.worker(&a.worker_url)?.get_cred(&a.eval_id).await?,
                    Some(a.eval_id.as_str()),
                ),
            };
            let endpoint = Some(a.endpoint.as_str()).filter(|e| !e.is_empty());
            let line = cred::open_credential(&sealed, &keys, endpoint, expect)?;
            Some(
                crucible_meter::read_credential(&mut line.as_bytes())
                    .map_err(|e| anyhow!("{e}"))?,
            )
        }
    };
    std::fs::create_dir_all(&a.out)?;
    crate::score::write_handoff(
        &ts, &a.taskset, &stages, &a.results, &store, &keys, &a.out, &to, cred, &a.model, &a.budget,
    )
    .await?;
    drop(keys);
    // What the scoring step runs.
    let bin = a.out.join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(std::env::current_exe()?, bin.join("crucible"))?;
    for d in ["scorers", "runners"] {
        copy_dir(&a.root.join(d), &a.out.join(d))?;
    }
    std::fs::create_dir_all(a.out.join("tools"))?;
    std::fs::copy(
        a.root.join("tools/sandbox-net.sh"),
        a.out.join("tools/sandbox-net.sh"),
    )?;
    std::fs::copy(
        a.root.join("config/pricing.json"),
        a.out.join("pricing.json"),
    )?;
    // Tests, checkpoints and the credential leave only sealed.
    for d in ["tests", "results"] {
        super::sealed_only(&a.out.join(d), false)?;
    }
    if a.out.join("cred.sealed").exists() {
        let c = std::fs::read(a.out.join("cred.sealed"))?;
        crucible_crypto::sealed_key_id(&c).map_err(|_| anyhow!("cred.sealed is not sealed"))?;
    }
    if let (Some(k), Some(p)) = (run_key, &a.key_out) {
        crate::write_private(p, k.to_secret_string().as_bytes())?;
    }
    Ok(())
}

/// Copy a directory tree (regular files and directories; symlinks are an error).
pub fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let t = e.file_type()?;
        let dst = to.join(e.file_name());
        if t.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else if t.is_file() {
            std::fs::copy(e.path(), &dst)?;
        } else {
            bail!("{} is not a regular file", e.path().display());
        }
    }
    Ok(())
}
