//! Registering an uploaded plugin (plugin-pack.yml, docs/plugins.md §14),
//! in three steps so that the plugin's code never runs where a platform
//! secret is:
//!
//! - `plugin-open` (platform key, Worker token, store read): opens the
//!   sealed upload, checks the package (`plugin.json`, `Dockerfile`, zip
//!   limits) without running anything from it, and re-seals it to a
//!   one-run key for the build. A refused package is reported to the
//!   Worker here.
//! - `plugin-build` (only the one-run key): builds the image through the
//!   execution backend and runs a minimal self-test through the generic
//!   shell (`scorers/_user/score.sh`): the package's `selftest/artifact`
//!   and `selftest/tests/` (or an empty file and directory) must give a
//!   valid `result.json`. Writes only the outcome (`outcome.json`).
//! - `plugin-report` (Worker token): delivers the outcome.
//!
//! The package's file names, build output and the reason for a refusal go
//! to the Worker (the uploader sees them), never to the step logs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use crucible_core::ScoreResult;
use crucible_core::plugins::UserPlugin;
use serde_json::{Value, json};

use super::{Common, Secret, Secrets};
use crate::executor::Executor;

#[derive(clap::Args)]
pub struct OpenArgs {
    #[command(flatten)]
    pub common: Common,
    /// `u-<16 hex>`.
    #[arg(long)]
    pub plugin_id: String,
    /// `blob:<sha256>` of the sealed upload (a zip).
    #[arg(long)]
    pub source: String,
    /// `<worker>/internal/plugins/<plugin_id>`; empty: do not report.
    #[arg(long, default_value = "")]
    pub results_url: String,
    /// Holds scorers/_user/score.sh.
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    #[arg(long)]
    pub store: String,
    /// Write the one-run key's private half here (0600).
    #[arg(long)]
    pub key_out: PathBuf,
    /// The build bundle: package.sealed, plugin.json (the pinned form),
    /// bin/crucible, scorers/_user/; `reported` when a refusal was
    /// delivered, `worker` (the validated Worker origin).
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(clap::Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub common: Common,
    /// The bundle of `plugin-open`.
    #[arg(long)]
    pub handoff: PathBuf,
    /// `<out>/outcome.json`.
    #[arg(long)]
    pub out: PathBuf,
}

#[derive(clap::Args)]
pub struct ReportArgs {
    #[command(flatten)]
    pub common: Common,
    #[arg(long)]
    pub plugin_id: String,
    /// The Worker (`https://...`; only its origin is used).
    #[arg(long)]
    pub worker_url: String,
    /// The pinned plugin (plugin-open's plugin.json), as JSON text.
    #[arg(long, default_value = "")]
    pub plugin: String,
    /// plugin-build's outcome.json, as JSON text; empty: the build did not
    /// finish.
    #[arg(long, default_value = "")]
    pub outcome: String,
}

fn source_hash(source: &str) -> Result<&str> {
    source
        .strip_prefix("blob:")
        .filter(|h| crucible_core::blob::is_sha256_hex(h))
        .ok_or_else(|| anyhow!("source must be blob:<sha256>"))
}

pub async fn open(a: Box<OpenArgs>, s: &Secrets) -> Result<()> {
    let id = a.plugin_id.trim();
    if !crucible_core::plugins::is_user_plugin_id(id) {
        bail!("plugin_id must be u-<16 hex>");
    }
    let hash = source_hash(&a.source)?;
    std::fs::create_dir_all(&a.out)?;
    let worker = match a.results_url.trim() {
        "" => None,
        u => {
            let origin = crate::worker::origin(u)?;
            if u != format!("{origin}/internal/plugins/{id}") {
                bail!("results_url must be https://<worker>/internal/plugins/{id}");
            }
            std::fs::write(a.out.join("worker"), &origin)?;
            Some(s.worker(&origin)?)
        }
    };
    let keys = s.keys(Secret::PlatformKey)?;
    let sealed = s.store(&a.store, false)?.get(hash).await?;
    let key_id =
        crucible_crypto::sealed_key_id(&sealed).map_err(|_| anyhow!("the upload is not sealed"))?;
    let zip = crucible_crypto::open(&keys, &sealed)?;
    drop(keys);
    let tmp = tempfile::tempdir()?;
    let checked = crate::user_plugin::unpack(&zip, tmp.path()).map(|(_, m)| m);
    let manifest = match (checked, &worker) {
        (Ok(m), _) => m,
        (Err(_), None) => bail!("the plugin was refused (the reason is not logged)"),
        (Err(e), Some(w)) => {
            println!("::error::the plugin was refused (the reason goes to the uploader)");
            let reason: String = format!("{e:#}")
                .replace(&format!("{}/", tmp.path().display()), "")
                .chars()
                .take(500)
                .collect();
            w.plugin_result(id, &json!({"status": "failed", "error": reason}))
                .await?;
            std::fs::write(a.out.join("reported"), "")?;
            bail!("plugin refused")
        }
    };
    let pinned = manifest.pin(
        id,
        crucible_core::BlobRef {
            sha256: hash.to_owned(),
            key_id,
        },
    );
    pinned.check().map_err(|e| anyhow!("{e}"))?;
    let run_key = crucible_crypto::PrivateKey::generate();
    std::fs::write(
        a.out.join("package.sealed"),
        crucible_crypto::seal(&run_key.public(), &zip)?,
    )?;
    drop(zip);
    std::fs::write(
        a.out.join("plugin.json"),
        serde_json::to_string(&json!({
            "plugin": pinned,
            "title": manifest.name,
            "description": manifest.description,
        }))?,
    )?;
    let bin = a.out.join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(std::env::current_exe()?, bin.join("crucible"))?;
    let shell = a.out.join(crate::score::USER_SCORER_SHELL);
    std::fs::create_dir_all(shell.parent().expect("has a parent"))?;
    std::fs::copy(a.root.join(crate::score::USER_SCORER_SHELL), &shell)?;
    crate::write_private(&a.key_out, run_key.to_secret_string().as_bytes())?;
    eprintln!("{} {id}: package checked", pinned.kind.as_str());
    Ok(())
}

/// Build and self-test; never fails for a reason of the plugin's own
/// (that is the outcome), only for the platform's.
pub async fn build(a: BuildArgs, s: &Secrets) -> Result<()> {
    let h = std::fs::canonicalize(&a.handoff)?;
    let keys = s.keys(Secret::RunKey)?;
    let exec = crate::executor::backend()?;
    super::check_caps(super::spec("plugin-build"), exec.caps())?;
    let info: Value = serde_json::from_slice(&std::fs::read(h.join("plugin.json"))?)?;
    let pinned: UserPlugin = serde_json::from_value(info["plugin"].clone())?;
    let zip = crucible_crypto::open(&keys, &std::fs::read(h.join("package.sealed"))?)
        .context("opening the package")?;
    let tmp = tempfile::tempdir()?;
    std::fs::create_dir_all(&a.out)?;
    let outcome = match crate::user_plugin::unpack(&zip, tmp.path()) {
        Err(e) => json!({"status": "failed", "error": format!("{e:#}")}),
        Ok((root, _)) => {
            let tag = crate::user_plugin::image_tag(pinned.kind.as_str(), &pinned.name);
            match crate::user_plugin::build(&exec, &root, &tag).await {
                Err(e) => json!({"status": "failed", "error": format!("{e:#}")}),
                Ok(()) => {
                    let r = selftest(&h, &root, &exec.image_ref(&tag));
                    exec.image_rm(&exec.image_ref(&tag)).await;
                    match r {
                        Ok(r) => json!({"status": "ready", "selftest": summary(&r)}),
                        Err(e) => json!({"status": "failed", "error": format!("self-test: {e:#}")}),
                    }
                }
            }
        }
    };
    let ok = outcome["status"] == "ready";
    let mut outcome = outcome;
    if let Some(e) = outcome["error"].as_str() {
        outcome["error"] = json!(e.chars().take(500).collect::<String>());
    }
    std::fs::write(a.out.join("outcome.json"), serde_json::to_string(&outcome)?)?;
    eprintln!(
        "plugin {}: {}",
        pinned.name,
        if ok {
            "built, self-test passed"
        } else {
            "refused (the reason goes to the uploader)"
        }
    );
    Ok(())
}

fn summary(r: &ScoreResult) -> Value {
    json!({
        "status": r.status,
        "score": r.score,
        "max": r.max,
        "detail": r.detail.chars().take(300).collect::<String>(),
    })
}

/// Run the image once through the generic shell on the package's example
/// (or empty inputs); a `result.json` that parses is a pass.
fn selftest(h: &Path, root: &Path, image: &str) -> Result<ScoreResult> {
    let work = tempfile::tempdir()?;
    let ex = root.join("selftest");
    let artifact = if ex.join("artifact").is_file() {
        ex.join("artifact")
    } else {
        let p = work.path().join("artifact");
        std::fs::write(&p, b"")?;
        p
    };
    let tests = if ex.join("tests").is_dir() {
        ex.join("tests")
    } else {
        let p = work.path().join("tests");
        std::fs::create_dir_all(&p)?;
        p
    };
    let out = work.path().join("result.json");
    let status = std::process::Command::new("bash")
        .arg(h.join(crate::score::USER_SCORER_SHELL))
        .arg("--artifact")
        .arg(&artifact)
        .arg("--tests")
        .arg(&tests)
        .arg("--out")
        .arg(&out)
        .args(["--visibility", "public"])
        .env("CRUCIBLE", std::env::current_exe()?)
        .env("CRUCIBLE_SCORER_IMAGE", image)
        .env("CRUCIBLE_USER_SCORER_TIMEOUT_S", "300")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        bail!("the shell failed ({status})");
    }
    let r: ScoreResult = serde_json::from_slice(&std::fs::read(&out)?)
        .map_err(|e| anyhow!("result.json is not a scorer result: {e}"))?;
    let r = r.finish(&Default::default());
    if r.status == crucible_core::ScoreStatus::Error
        && r.detail.starts_with("the plugin wrote no result.json")
    {
        bail!("{}", r.detail);
    }
    Ok(r)
}

pub async fn report(a: ReportArgs, s: &Secrets) -> Result<()> {
    let id = a.plugin_id.trim();
    if !crucible_core::plugins::is_user_plugin_id(id) {
        bail!("plugin_id must be u-<16 hex>");
    }
    let w = s.worker(&crate::worker::origin(a.worker_url.trim())?)?;
    let outcome: Value = match a.outcome.trim() {
        "" => {
            json!({"status": "failed", "error": "building failed on the platform side; please retry"})
        }
        o => serde_json::from_str(o).context("outcome")?,
    };
    let body = if outcome["status"] == "ready" {
        let info: Value = serde_json::from_str(a.plugin.trim()).context("plugin")?;
        json!({
            "status": "ready",
            "plugin": info["plugin"],
            "title": info["title"],
            "description": info["description"],
            "selftest": outcome["selftest"],
        })
    } else {
        json!({
            "status": "failed",
            "error": outcome["error"].as_str().unwrap_or("refused"),
        })
    };
    w.plugin_result(id, &body).await?;
    eprintln!("plugin {id}: {} reported", body["status"]);
    Ok(())
}
