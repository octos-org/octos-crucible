//! `crucible step publish`: runs no agent or test code. Stores the sealed
//! outputs as blobs, builds the manifest (scores, usage, times), packs the
//! submitter's download zip (workers-kv credentials; skipped when no
//! replica left an output), archives the
//! manifest sealed to the platform key, and, as asked: commits it in clear
//! to the data branch (only if score_public), delivers it to the Worker.
//! Writes `<out>/manifest.json`.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use super::{Common, Secret, Secrets};
use crate::{ManifestArgs, PriceArgs};

#[derive(clap::Args)]
pub struct Args {
    #[command(flatten)]
    pub common: Common,
    /// Holds config/keys.json and config/pricing.json.
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// `agent` or `app`.
    #[arg(long, default_value = "agent")]
    pub mode: String,
    #[arg(long)]
    pub eval_id: String,
    #[arg(long)]
    pub taskset: PathBuf,
    /// Agent mode: the first N stages ran.
    #[arg(long)]
    pub stages: Option<usize>,
    /// App mode: the stage (1-based) the upload was scored on.
    #[arg(long)]
    pub stage: Option<usize>,
    /// App mode: the sealed upload's blob.
    #[arg(long)]
    pub app_blob: Option<String>,
    #[arg(long, default_value = "")]
    pub model: String,
    #[arg(long, default_value_t = 1)]
    pub replicas: u32,
    /// `<dir>/<replica>/<stage>/*.sealed` (gen-r* bundles).
    #[arg(long)]
    pub results: PathBuf,
    /// `<dir>/<replica>/<stage>/score.json` (scores-r* bundles).
    #[arg(long)]
    pub scores: PathBuf,
    #[arg(long, default_value = "")]
    pub owner: String,
    #[arg(long, default_value = "false")]
    pub score_public: String,
    /// Run-wide caps as `crucible plan` writes them (their `price`).
    #[arg(long, default_value = "")]
    pub budget: String,
    #[arg(long)]
    pub store: String,
    /// Wall times from this GitHub run's job and step timestamps
    /// (GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT).
    #[arg(long)]
    pub github_jobs: bool,
    /// Commit the manifest to the data branch when score_public (GitHub).
    #[arg(long)]
    pub data_branch: bool,
    /// Deliver the manifest to this Worker.
    #[arg(long, default_value = "")]
    pub deliver: String,
    /// Pack the download zip (the credential is from this Worker's KV).
    #[arg(long, default_value = "")]
    pub download_zip: String,
    /// Recorded as the manifest's `timing_source` (e.g. `local`).
    #[arg(long)]
    pub timing_source: Option<String>,
    #[arg(long)]
    pub out: PathBuf,
}

pub async fn run(a: Args, s: &Secrets) -> Result<()> {
    let keys = s.keys(Secret::PlatformKey)?;
    let agent = match a.mode.as_str() {
        "agent" => true,
        "app" => false,
        _ => bail!("--mode must be agent or app"),
    };
    std::fs::create_dir_all(&a.out)?;
    std::fs::create_dir_all(&a.results)?;
    std::fs::create_dir_all(&a.scores)?;
    let ts = crate::taskset_cmd::load(&a.taskset)?;
    let wstore = s.store(&a.store, true)?;
    if agent {
        let files = crate::publish::sealed_files(&a.results)?;
        for f in &files {
            let data = std::fs::read(f)?;
            crucible_crypto::sealed_key_id(&data)
                .with_context(|| format!("{} is not sealed", f.display()))?;
            let want = crucible_store::sha256_hex(&data);
            let got = wstore.put(&data).await?;
            if got != want {
                bail!("store returned {got} for {want}");
            }
        }
        eprintln!("stored {} sealed files", files.len());
    } else {
        // The upload's blob reference (hash + key id) goes into the manifest.
        let (Some(k), Some(h)) = (a.stage, &a.app_blob) else {
            bail!("app mode needs --stage and --app-blob");
        };
        let st = ts
            .stages
            .get(k.wrapping_sub(1))
            .ok_or_else(|| anyhow::anyhow!("--stage out of range"))?;
        let d = a.results.join("1").join(&st.id);
        std::fs::create_dir_all(&d)?;
        std::fs::write(
            d.join("checkpoint.sealed"),
            s.store(&a.store, false)?.get(h).await?,
        )?;
    }

    let tmp = tempfile::tempdir()?;
    let jobs = if a.github_jobs {
        let p = tmp.path().join("jobs.json");
        std::fs::write(&p, github_jobs(&s.text(Secret::RepoToken)?).await?)?;
        Some(p)
    } else {
        None
    };
    let price_json = match a.budget.trim() {
        "" => None,
        b => serde_json::from_str::<serde_json::Value>(b)?
            .get("price")
            .filter(|p| !p.is_null())
            .map(|p| p.to_string()),
    };
    let manifest_path = a.out.join("manifest.json");
    let mut m = crate::build_manifest(
        &ManifestArgs {
            eval_id: a.eval_id.clone(),
            taskset: a.taskset.clone(),
            mode: a.mode.clone(),
            stages: a.stages,
            stage: a.stage,
            model: a.model.clone(),
            scores: Some(a.scores.clone()),
            identity_env: vec![],
            results: a.results.clone(),
            replicas: a.replicas,
            jobs,
            run_step: "Run agent stages".into(),
            owner: a.owner.clone(),
            score_public: a.score_public.clone(),
            repository: std::env::var("GITHUB_REPOSITORY").ok(),
            run_id: std::env::var("GITHUB_RUN_ID")
                .ok()
                .and_then(|v| v.parse().ok()),
            run_attempt: std::env::var("GITHUB_RUN_ATTEMPT")
                .ok()
                .and_then(|v| v.parse().ok()),
            price: PriceArgs {
                pricing: a.root.join("config/pricing.json"),
                price_json,
            },
            out: manifest_path.clone(),
        },
        &keys,
    )?;
    m.timing_source = a.timing_source.clone();
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&m)? + "\n")?;
    print_summary(&m);

    if !a.download_zip.is_empty() {
        let w = s.worker(&a.download_zip)?;
        crate::download_zip(&w, &a.eval_id, &a.results, &keys, &wstore, &manifest_path).await?;
    }
    drop(keys);

    // Results sink (a): sealed archive, always.
    let manifest = std::fs::read(&manifest_path)?;
    let key = crate::keys::current_public_key(&a.root.join("config/keys.json"))?;
    let sealed = crucible_crypto::seal(&key, &manifest)?;
    std::fs::write(a.out.join("manifest.sealed"), &sealed)?;
    let sha = wstore.put(&sealed).await?;
    eprintln!("manifest archived as blob {sha}");
    let m: crucible_core::Manifest = serde_json::from_slice(&manifest)?;
    super::step_summary(&format!(
        "## eval {}\n\nmanifest blob: `{sha}`\n\ntotal_score: {}\n",
        a.eval_id,
        m.total_score.map_or("none".into(), |t| t.to_string())
    ));
    // (b) the data branch, only if score_public.
    if a.data_branch && a.score_public.trim() == "true" {
        data_branch(&a.eval_id, &manifest, &s.text(Secret::RepoToken)?)?;
    }
    // (c) the Worker.
    if !a.deliver.is_empty() {
        let raw: serde_json::Value = serde_json::from_slice(&manifest)?;
        let scored = m
            .replicas
            .iter()
            .flat_map(|r| &r.stages)
            .any(|s| s.score.as_ref().is_some_and(|s| s.status.is_scored()));
        let status = if scored { "done" } else { "failed" };
        s.worker(&a.deliver)?
            .results(&a.eval_id, &raw, status)
            .await?;
        eprintln!("results delivered ({status})");
    }
    Ok(())
}

/// The public log line: ids, numbers, scores.
fn print_summary(m: &crucible_core::Manifest) {
    let v = serde_json::to_value(m).unwrap_or_default();
    let reps: Vec<serde_json::Value> = v["replicas"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            serde_json::json!({
                "replica": r["replica"], "failure": r["failure"],
                "stages": r["stages"].as_array().into_iter().flatten().map(|s| serde_json::json!({
                    "stage": s["stage"], "score": s["score"], "wall_s": s["wall_s"],
                    "ended": s["ended"], "checkpoint_source": s["checkpoint_source"],
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "eval_id": v["eval_id"], "mode": v["mode"], "stages_run": v["stages_run"],
            "total_score": v["total_score"], "replicas": reps,
        })
    );
}

/// `GET /repos/{repo}/actions/runs/{id}/attempts/{n}/jobs` of this run.
async fn github_jobs(token: &str) -> Result<String> {
    let var = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
    let url = format!(
        "https://api.github.com/repos/{}/actions/runs/{}/attempts/{}/jobs?per_page=100",
        var("GITHUB_REPOSITORY")?,
        var("GITHUB_RUN_ID")?,
        var("GITHUB_RUN_ATTEMPT")?
    );
    let client = reqwest::Client::builder()
        .user_agent(concat!("crucible/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let mut last = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(3 * attempt)).await;
        }
        match client
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(r.text().await?),
            Ok(r) => last = Some(anyhow::anyhow!("jobs API: {}", r.status())),
            Err(e) => last = Some(e.into()),
        }
    }
    Err(last.expect("tried"))
}

/// Commit `evals/<eval_id>.json` (without `download`) to the data branch.
fn data_branch(eval_id: &str, manifest: &[u8], token: &str) -> Result<()> {
    let repo = std::env::var("GITHUB_REPOSITORY").context("GITHUB_REPOSITORY is not set")?;
    let d = tempfile::tempdir()?;
    let dir = d.path();
    let mut m: crucible_core::Manifest = serde_json::from_slice(manifest)?;
    m.download = None;
    // Details are for the submitter only.
    for r in &mut m.replicas {
        r.failure_detail = None;
    }
    let auth = format!(
        "http.https://github.com/.extraheader=AUTHORIZATION: basic {}",
        crate::cred::base64_encode(format!("x-access-token:{token}").as_bytes())
    );
    let git = |args: &[&str]| -> Result<bool> {
        Ok(std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", &auth])
            .args(args)
            .stdin(std::process::Stdio::null())
            .status()?
            .success())
    };
    let must = |ok: bool, what: &str| {
        if ok {
            Ok(())
        } else {
            Err(anyhow::anyhow!("git {what} failed"))
        }
    };
    must(git(&["init", "-q"])?, "init")?;
    must(
        git(&[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{repo}.git"),
        ])?,
        "remote add",
    )?;
    for attempt in 1..=5u64 {
        if git(&["ls-remote", "--exit-code", "origin", "refs/heads/data"])? {
            must(
                git(&["fetch", "-q", "--depth", "1", "origin", "data"])?,
                "fetch",
            )?;
            must(
                git(&["checkout", "-q", "-B", "data", "FETCH_HEAD"])?,
                "checkout",
            )?;
        } else {
            must(git(&["checkout", "-q", "--orphan", "data"])?, "checkout")?;
        }
        std::fs::create_dir_all(dir.join("evals"))?;
        std::fs::write(
            dir.join(format!("evals/{eval_id}.json")),
            serde_json::to_string_pretty(&m)? + "\n",
        )?;
        must(git(&["add", &format!("evals/{eval_id}.json")])?, "add")?;
        if !git(&[
            "-c",
            "user.name=crucible-bot",
            "-c",
            "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit",
            "-q",
            "-m",
            &format!("eval {eval_id}"),
        ])? {
            eprintln!("manifest unchanged");
            return Ok(());
        }
        if git(&["push", "-q", "origin", "HEAD:refs/heads/data"])? {
            eprintln!("published evals/{eval_id}.json on data");
            return Ok(());
        }
        eprintln!("push raced, retrying ({attempt})");
        std::thread::sleep(std::time::Duration::from_secs(attempt * 3));
    }
    bail!("could not push to the data branch")
}
