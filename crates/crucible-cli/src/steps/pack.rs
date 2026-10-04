//! `crucible step pack`: register a user-uploaded taskset (taskset-pack.yml).
//! Holds the platform key, runs no uploaded code: opens the sealed upload,
//! checks it with the user rules, cuts every stage into a sealed inputs and
//! tests blob, and reports the taskset.json (or why it was refused) to the
//! Worker. File names of the upload and the reason go to the Worker only,
//! never to this step's log.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use super::{Common, Secret, Secrets};

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: Common,
    /// `u-<16 hex>`.
    #[arg(long)]
    pub taskset_id: String,
    /// `blob:<sha256>` of the sealed upload (a zip).
    #[arg(long)]
    pub source: String,
    /// `<worker>/internal/tasksets/<taskset_id>`; empty: do not report.
    #[arg(long, default_value = "")]
    pub results_url: String,
    /// config/keys.json (the current public key seals the blobs).
    #[arg(long, default_value = "config/keys.json")]
    pub keys: PathBuf,
    #[arg(long)]
    pub store: String,
    /// `<out>/taskset.json`; `<out>/reported` once the Worker has the outcome.
    #[arg(long)]
    pub out: PathBuf,
}

pub async fn run(a: Box<Args>, s: &Secrets) -> Result<()> {
    let id = a.taskset_id.trim();
    if !crucible_core::taskset::is_user_taskset_id(id) {
        bail!("taskset_id must be u-<16 hex>");
    }
    let hash = a
        .source
        .strip_prefix("blob:")
        .filter(|h| h.len() == 64 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .ok_or_else(|| anyhow!("source must be blob:<sha256>"))?;
    let worker = match a.results_url.trim() {
        "" => None,
        u => {
            let origin = crate::worker::origin(u)?;
            if u != format!("{origin}/internal/tasksets/{id}") {
                bail!("results_url must be https://<worker>/internal/tasksets/{id}");
            }
            // The validated Worker, for the workflow's failure report.
            std::fs::create_dir_all(&a.out)?;
            std::fs::write(a.out.join("worker"), &origin)?;
            Some(s.worker(&origin)?)
        }
    };
    let packed = pack(&a, id, hash, s, worker.as_ref()).await;
    let Some(w) = worker else {
        return match packed {
            Ok(_) => Ok(()),
            Err(_) => bail!("the taskset was refused (the reason is not logged)"),
        };
    };
    match packed {
        Ok(ts) => {
            eprintln!(
                "{}",
                serde_json::json!({"name": ts.name, "total_time_limit_s": ts.total_time_limit_s, "stages": ts.stages.len()})
            );
            w.taskset_result(id, &serde_json::json!({"status": "ready", "taskset": ts}))
                .await?;
            std::fs::write(a.out.join("reported"), "")?;
            Ok(())
        }
        Err(e) => {
            println!("::error::the taskset was refused (the reason goes to the uploader)");
            let reason: String = format!("{e:#}").chars().take(500).collect();
            w.taskset_result(
                id,
                &serde_json::json!({"status": "failed", "error": reason}),
            )
            .await?;
            bail!("taskset refused")
        }
    }
}

async fn pack(
    a: &Args,
    id: &str,
    hash: &str,
    s: &Secrets,
    worker: Option<&crate::worker::Worker>,
) -> Result<crucible_core::TaskSet> {
    let keys = s.keys(Secret::PlatformKey)?;
    let zip = crucible_crypto::open(&keys, &s.store(&a.store, false)?.get(hash).await?)?;
    drop(keys);
    let key = crate::keys::current_public_key(&a.keys)?;
    let tmp = tempfile::tempdir()?;
    let root = crate::taskset_cmd::unpack_source_zip(&zip, tmp.path())?;
    // Uploaded plugins its scorers name: the Worker says whether this
    // taskset's owner may use them (their own, or public) and pins them.
    let mut user_plugins = Vec::new();
    for pid in crate::taskset_cmd::PackSource::read(&root.join("source.json"))
        .map_err(|e| {
            anyhow!(
                "{}",
                format!("{e:#}").replace(&format!("{}/", root.display()), "")
            )
        })?
        .user_plugin_refs()
    {
        let w = worker.ok_or_else(|| anyhow!("uploaded plugins need the Worker (results_url)"))?;
        user_plugins.push(
            w.get_user_plugin(&pid, id)
                .await
                .map_err(|e| anyhow!("scorer {pid}: not a ready plugin you may use ({e:#})"))?,
        );
    }
    // Errors name files relative to the upload, not this machine.
    let mut ts = crate::taskset_cmd::pack_with(
        &root.join("source.json"),
        &root,
        true,
        &user_plugins,
        &key,
        &s.store(&a.store, true)?,
        false,
    )
    .await
    .map_err(|e| {
        anyhow!(
            "{}",
            format!("{e:#}").replace(&format!("{}/", root.display()), "")
        )
    })?;
    ts.title = Some(std::mem::replace(&mut ts.name, id.to_owned()));
    std::fs::create_dir_all(&a.out)?;
    std::fs::write(
        a.out.join("taskset.json"),
        serde_json::to_string_pretty(&ts)? + "\n",
    )?;
    Ok(ts)
}
