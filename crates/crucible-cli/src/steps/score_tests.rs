//! `crucible step score-tests`: the only place test code runs. Holds only
//! the handoff's one-run key (its spec lists nothing else). Builds the
//! container plugins the taskset uses, brings up the sandbox network when
//! the handoff carries a model credential (the meter's port only), and
//! runs `crucible score` on the handoff. Only `score.json` and the scoring
//! meters' usage logs are written to `--out`.

use std::path::PathBuf;

use anyhow::{Result, bail};

use super::{Common, Secret, Secrets};
use crate::sandbox::SandboxNet;

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: Common,
    /// The handoff bundle.
    #[arg(long)]
    pub handoff: PathBuf,
    /// Score only this replica.
    #[arg(long)]
    pub replica: Option<u32>,
    /// `<out>/<replica>/<stage>/score.json`.
    #[arg(long)]
    pub out: PathBuf,
}

const METER_PORT: u16 = 8787;

pub async fn run(a: Args, s: &Secrets) -> Result<()> {
    let h = std::fs::canonicalize(&a.handoff)?;
    let ts = crate::taskset_cmd::load(&h.join("taskset.json"))?;
    ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
    let keys = s.keys(Secret::RunKey)?;

    // The container plugins the taskset uses, from the registry compiled
    // into crucible; `crucible score` hands each its image.
    let reg = crucible_core::plugins::registry();
    for v in ts.plugin_versions() {
        let Some(p) = reg
            .plugins
            .iter()
            .find(|p| p.kind.as_str() == v.kind && p.name == v.name)
        else {
            bail!("{} {} is not registered", v.kind, v.name);
        };
        if p.is_builtin() {
            continue;
        }
        let dir = h.join(&p.implementation);
        if !dir.join("score.sh").is_file() && !dir.join("run.sh").is_file() {
            bail!("{} {} missing in the handoff", v.kind, p.name);
        }
        if dir.join("image").is_dir() {
            let tag = format!("crucible-{}-{}:run", v.kind, p.name);
            let st = crate::build::docker()
                .args(["build", "-q", "-t", &tag])
                .arg(dir.join("image"))
                .stdout(std::process::Stdio::null())
                .status()?;
            if !st.success() {
                bail!("{} {}: image build failed", v.kind, p.name);
            }
        }
        eprintln!("{} {} ready", v.kind, p.name);
    }

    let cred = h.join("cred.sealed");
    let net = if cred.is_file() {
        Some(SandboxNet::up(&METER_PORT.to_string())?)
    } else {
        eprintln!("no model credential in the handoff");
        None
    };
    let model = match &net {
        Some(n) => Some(crate::score::model_setup(
            &cred,
            &h.join("model.json"),
            &h.join("pricing.json"),
            &keys,
            &n.network,
            &n.gateway,
            METER_PORT,
        )?),
        None => None,
    };
    let stages: Vec<usize> = (0..ts.stages.len()).collect();
    let results = h.join("results");
    std::fs::create_dir_all(&results)?;
    std::fs::create_dir_all(&a.out)?;
    let done = crate::score::score(&crate::score::ScoreOpts {
        taskset: &ts,
        stages,
        results: &results,
        out: &a.out,
        tests: crate::score::TestsFrom::Dir(&h.join("tests")),
        keys: &keys,
        plugins_root: &h,
        scrub_env: &[],
        replica: a.replica,
        model: model.as_ref(),
    })
    .await;
    drop(model);
    drop(net);
    eprintln!("scored {} stage checkpoints", done?.len());
    Ok(())
}
