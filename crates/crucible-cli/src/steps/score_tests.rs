//! `crucible step score-tests`: the only place test code runs. Holds only
//! the handoff's one-run key (its spec lists nothing else). Builds the
//! container plugins the taskset uses, brings up the sandbox network when
//! the handoff carries a model credential (the meter's port only), and
//! runs `crucible score` on the handoff. Only `score.json` and the scoring
//! meters' usage logs are written to `--out`.
//!
//! On failure only a category is printed and written to
//! `<out>/<replica>/failure.json` (`{"category"}`; this machine holds the
//! tests, so no details leave it); publish puts it into the manifest.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use super::{Common, Secret, Secrets};
use crate::executor::{BuildSpec, Executor};

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
    /// Label of this step's sandbox (`crucible.run=<label>`); what an
    /// earlier attempt with the same label left is removed first.
    #[arg(long)]
    pub run_label: Option<String>,
    /// `<out>/<replica>/<stage>/score.json`.
    #[arg(long)]
    pub out: PathBuf,
}

const METER_PORT: u16 = 8787;

/// Failure categories (the manifest's `replicas[].failure`).
pub const SCORER_BUILD: &str = "scorer image build failed";
pub const SCORING: &str = "scoring failed";

pub async fn run(a: Args, s: &Secrets) -> Result<()> {
    let phase = std::cell::Cell::new(SCORING);
    let r = score_tests(&a, s, &phase).await;
    r.map_err(|_| {
        let category = phase.get();
        let dir = match a.replica {
            Some(r) => a.out.join(r.to_string()),
            None => a.out.clone(),
        };
        let f = serde_json::json!({ "category": category });
        if std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join("failure.json"), f.to_string()))
            .is_err()
        {
            eprintln!("could not record the failure");
        }
        eprintln!("score-tests: {category}");
        anyhow!("{category}")
    })
}

async fn score_tests(a: &Args, s: &Secrets, phase: &std::cell::Cell<&'static str>) -> Result<()> {
    let h = std::fs::canonicalize(&a.handoff)?;
    let ts = crate::taskset_cmd::load(&h.join("taskset.json"))?;
    ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
    let keys = s.keys(Secret::RunKey)?;
    let exec = crate::executor::backend()?;
    super::check_caps(super::spec("score-tests"), exec.caps())?;

    // The container plugins the taskset uses: from the registry compiled
    // into crucible, or uploaded (their sealed packages are in the
    // handoff); `crucible score` hands each its image.
    phase.set(SCORER_BUILD);
    for v in ts.plugin_versions() {
        let Some(p) = ts.plugins_of_kind(&v.kind, &v.name) else {
            bail!("{} {} is not registered", v.kind, v.name);
        };
        if p.is_builtin() {
            continue;
        }
        let tag = format!("crucible-{}-{}:run", v.kind, p.name);
        if p.is_user() {
            let sealed = h.join("plugins").join(format!("{}.sealed", p.name));
            let zip = crucible_crypto::open(&keys, &std::fs::read(&sealed)?)
                .map_err(|_| anyhow!("{} {}: package missing in the handoff", v.kind, p.name))?;
            let tmp = tempfile::tempdir()?;
            let (root, _) = crate::user_plugin::unpack(&zip, tmp.path())?;
            drop(zip);
            crate::user_plugin::build(&exec, &root, &tag)
                .await
                .map_err(|e| anyhow!("{} {}: {e:#}", v.kind, p.name))?;
            eprintln!("{} {} (uploaded) ready", v.kind, p.name);
            continue;
        }
        let dir = h.join(&p.implementation);
        if !dir.join("score.sh").is_file() && !dir.join("run.sh").is_file() {
            bail!("{} {} missing in the handoff", v.kind, p.name);
        }
        if dir.join("image").is_dir() {
            let log = tempfile::NamedTempFile::new()?;
            let b = BuildSpec {
                dir: dir.join("image"),
                tag: exec.image_ref(&tag),
                quiet: true,
                ..Default::default()
            };
            if let Err(e) = exec.build(&b, log.path()).await {
                bail!("{} {}: image build failed: {e:#}", v.kind, p.name);
            }
        }
        eprintln!("{} {} ready", v.kind, p.name);
    }

    phase.set(SCORING);
    let label = match &a.run_label {
        Some(l) if !l.is_empty() => l.clone(),
        _ => format!("score-{}", std::process::id()),
    };
    exec.cleanup(&label);
    let cred = h.join("cred.sealed");
    let net = if cred.is_file() {
        Some(exec.sandbox(&[METER_PORT], &label)?)
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
            &n.host,
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
        run_label: Some(&label),
    })
    .await;
    drop(model);
    drop(net);
    exec.cleanup(&label);
    eprintln!("scored {} stage checkpoints", done?.len());
    Ok(())
}
