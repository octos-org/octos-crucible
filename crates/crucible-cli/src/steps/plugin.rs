//! Registering an uploaded plugin (plugin-pack.yml, docs/plugins.md §14),
//! in three steps so that the plugin's code never runs where a platform
//! secret is:
//!
//! - `plugin-open` (platform key, Worker token, store read): opens the
//!   sealed upload, checks the package (`plugin.json`, `Dockerfile` rules,
//!   zip limits) without running anything from it, and re-seals it to a
//!   one-run key for the build; seals the review material (file list,
//!   Dockerfile, text files) to the platform key. A refused package is
//!   reported to the Worker here.
//! - `plugin-build` (only the one-run key): builds the image through the
//!   execution backend with RUN steps on a sandbox network whose only way
//!   out is the egress proxy (the allowlist of `config/egress.json`),
//!   within a time and size limit; runs a minimal self-test through the
//!   generic shell (`scorers/_user/score.sh`): the package's
//!   `selftest/artifact` and `selftest/tests/` (or an empty file and
//!   directory) must give a valid `result.json`. Writes the image (`docker
//!   save`, gzip) and the outcome, both sealed to the platform key
//!   (`image.sealed`, `outcome.sealed`).
//! - `plugin-report` (platform key, Worker token, store write): opens the
//!   outcome, checks it belongs to this plugin and package, computes the
//!   image id from the image itself (never trusting the build's claim),
//!   stores the image, and registers it with the Worker.
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
    /// The bundle of `plugin-open` (plugin.json, review.sealed); absent:
    /// the package was not opened.
    #[arg(long)]
    pub handoff: Option<PathBuf>,
    /// The output of `plugin-build` (outcome.sealed, image.sealed); absent
    /// or empty: the build did not finish.
    #[arg(long)]
    pub outcome: Option<PathBuf>,
    /// Where the image is stored (with `--outcome`).
    #[arg(long, default_value = "")]
    pub store: String,
}

/// Port of the build's egress proxy on the sandbox host.
const EGRESS_PORT: u16 = 3128;
/// Wall clock of an image build.
const BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);
/// Text the review material may carry (the Worker takes 128 KB bodies).
const REVIEW_BUDGET: usize = 48 * 1024;

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
    let platform = keys[0].public();
    drop(keys);
    let tmp = tempfile::tempdir()?;
    let checked = crate::user_plugin::unpack(&zip, tmp.path());
    let (manifest, review) = match (checked, &worker) {
        (Ok((root, m)), _) => (m, crate::user_plugin::review_material(&root, REVIEW_BUDGET)?),
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
    // For the build: where its outputs are sealed to, its egress allowlist.
    std::fs::write(a.out.join("platform.pub"), platform.to_string())?;
    std::fs::write(
        a.out.join("review.sealed"),
        crucible_crypto::seal(&platform, &serde_json::to_vec(&review)?)?,
    )?;
    std::fs::copy(a.root.join("config/egress.json"), a.out.join("egress.json"))?;
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
    let platform: crucible_crypto::PublicKey =
        std::fs::read_to_string(h.join("platform.pub"))?.trim().parse()?;
    let zip = crucible_crypto::open(&keys, &std::fs::read(h.join("package.sealed"))?)
        .context("opening the package")?;
    drop(keys);
    let tmp = tempfile::tempdir()?;
    std::fs::create_dir_all(&a.out)?;
    let egress_log = tmp.path().join("egress.jsonl");
    let mut outcome = match crate::user_plugin::unpack(&zip, &tmp.path().join("pkg")) {
        Err(e) => json!({"status": "failed", "error": format!("{e:#}")}),
        Ok((root, _)) => {
            let tag = crate::user_plugin::image_tag(pinned.kind.as_str(), &pinned.name);
            let image = exec.image_ref(&tag);
            let built = isolated_build(&exec, &h, &root, &tag, &egress_log).await;
            let r = match built {
                Err(e) => Err(e),
                Ok(()) => keep_image(&exec, &h, &root, &image, &platform, &a.out).await,
            };
            exec.image_rm(&image).await;
            match r {
                Ok(v) => v,
                Err(e) => json!({"status": "failed", "error": format!("{e:#}")}),
            }
        }
    };
    let ok = outcome["status"] == "ready";
    if let Some(e) = outcome["error"].as_str() {
        outcome["error"] = json!(e.chars().take(500).collect::<String>());
    }
    if !ok {
        let _ = std::fs::remove_file(a.out.join("image.sealed"));
    }
    let egress = egress_summary(&egress_log);
    // Allowed hosts are allowlist entries; denied ones only go to the
    // uploader.
    eprintln!(
        "build egress: allowed {:?}, denied {} connection(s)",
        egress["allowed"], egress["denied"]
    );
    outcome["plugin_id"] = json!(pinned.name);
    outcome["package"] = json!(pinned.blob.sha256);
    outcome["egress"] = egress;
    std::fs::write(
        a.out.join("outcome.sealed"),
        crucible_crypto::seal(&platform, &serde_json::to_vec(&outcome)?)?,
    )?;
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

/// The image build: RUN steps on a sandbox network that reaches only
/// this step's egress proxy (allowlisted hosts, HTTPS); base images are
/// pulled by the daemon from the registries `check_dockerfile` allows.
async fn isolated_build(
    exec: &crate::executor::Backend,
    h: &Path,
    root: &Path,
    tag: &str,
    egress_log: &Path,
) -> Result<()> {
    let label = format!("plugin-build-{}", std::process::id());
    exec.cleanup(&label);
    let sbx = exec.sandbox(&[EGRESS_PORT], &label)?;
    std::fs::File::create(egress_log)?;
    let cfg = crucible_egress::EgressConfig::from_json(
        &std::fs::read_to_string(h.join("egress.json"))?,
        egress_log.to_path_buf(),
    )?;
    let listener = tokio::net::TcpListener::bind((sbx.host.as_str(), EGRESS_PORT))
        .await
        .with_context(|| format!("egress: binding {}:{EGRESS_PORT}", sbx.host))?;
    let proxy = tokio::spawn(crucible_egress::serve(listener, cfg));
    let url = format!("http://{}:{EGRESS_PORT}", sbx.host);
    let build_args = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"]
        .iter()
        .map(|k| format!("{k}={url}"))
        .collect();
    let r = crate::user_plugin::build(
        exec,
        root,
        tag,
        crate::user_plugin::BuildLimits {
            network: Some(sbx.network.clone()),
            build_args,
            limits: vec![
                ("--memory".into(), "4g".into()),
                ("--memory-swap".into(), "4g".into()),
            ],
            timeout: Some(BUILD_TIMEOUT),
        },
    )
    .await;
    proxy.abort();
    drop(sbx);
    exec.cleanup(&label);
    r
}

/// Size check, self-test, then the image (`docker save`, gzip) sealed to
/// the platform key in `<out>/image.sealed`; the outcome.
async fn keep_image(
    exec: &crate::executor::Backend,
    h: &Path,
    root: &Path,
    image: &str,
    platform: &crucible_crypto::PublicKey,
    out: &Path,
) -> Result<Value> {
    let saved = tempfile::NamedTempFile::new()?;
    exec.image_save(image, saved.path())
        .await
        .context("saving the image")?;
    let size = std::fs::metadata(saved.path())?.len();
    if size > crate::image_archive::MAX_IMAGE_BYTES {
        bail!(
            "the image is {} MB, more than the {} MB allowed",
            size >> 20,
            crate::image_archive::MAX_IMAGE_BYTES >> 20
        );
    }
    let archive = crate::image_archive::Archive::parse(std::fs::read(saved.path())?)?;
    drop(saved);
    let r = selftest(h, root, image)?;
    let id = archive.id.clone();
    let gz = crate::image_archive::gzip(&archive.into_bytes())?;
    std::fs::write(out.join("image.sealed"), crucible_crypto::seal(platform, &gz)?)?;
    Ok(json!({
        "status": "ready",
        "selftest": summary(&r),
        "image": {"id": id, "bytes": size},
    }))
}

/// Hosts the build reached (allowed) and how many connections were refused.
fn egress_summary(log: &Path) -> Value {
    let mut allowed = std::collections::BTreeSet::new();
    let mut denied = std::collections::BTreeSet::new();
    let mut refused = 0u64;
    for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
        let Ok(r) = serde_json::from_str::<crucible_egress::EgressRecord>(line) else {
            continue;
        };
        if r.allowed {
            allowed.insert(r.host);
        } else {
            refused += 1;
            denied.insert(r.host);
        }
    }
    json!({"allowed": allowed, "denied": refused, "denied_hosts": denied.into_iter().take(20).collect::<Vec<_>>()})
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
    let body = match verify(&a, id, s).await {
        Ok(b) => b,
        Err(e) => {
            // The platform's own failures and a build that did not deliver
            // a valid outcome: the reason is ours, not the plugin's.
            eprintln!("plugin {id}: outcome refused: {e:#}");
            json!({"status": "failed", "error": format!("building failed on the platform side; please retry ({})", short(&e))})
        }
    };
    w.plugin_result(id, &body).await?;
    eprintln!("plugin {id}: {} reported", body["status"]);
    Ok(())
}

fn short(e: &anyhow::Error) -> String {
    e.to_string().chars().take(200).collect()
}

/// The Worker's body from the build's sealed outcome, after checking it.
async fn verify(a: &ReportArgs, id: &str, s: &Secrets) -> Result<Value> {
    let (Some(h), Some(o)) = (&a.handoff, &a.outcome) else {
        bail!("the build did not finish");
    };
    let sealed = std::fs::read(o.join("outcome.sealed")).context("the build left no outcome")?;
    let keys = s.keys(Secret::PlatformKey)?;
    let outcome: Value = serde_json::from_slice(
        &crucible_crypto::open(&keys, &sealed)
            .map_err(|_| anyhow!("the outcome is not sealed to the platform key"))?,
    )
    .context("the outcome is not JSON")?;
    let info: Value = serde_json::from_slice(&std::fs::read(h.join("plugin.json"))?)?;
    let mut pinned: UserPlugin = serde_json::from_value(info["plugin"].clone())?;
    if pinned.name != id
        || outcome["plugin_id"] != json!(id)
        || outcome["package"] != json!(pinned.blob.sha256)
    {
        bail!("the outcome is not this plugin's");
    }
    let egress = outcome["egress"].clone();
    if outcome["status"] != "ready" {
        let mut e = outcome["error"].as_str().unwrap_or("refused").to_owned();
        if let Some(d) = egress["denied_hosts"].as_array().filter(|d| !d.is_empty()) {
            let hosts: Vec<&str> = d.iter().filter_map(|v| v.as_str()).collect();
            e = format!("{e}\n(the build proxy refused: {})", hosts.join(", "));
        }
        return Ok(json!({"status": "failed", "error": e.chars().take(500).collect::<String>()}));
    }
    // The image: its id is computed here, from the image itself.
    let sealed = std::fs::read(o.join("image.sealed")).context("the build left no image")?;
    let key_id = crucible_crypto::sealed_key_id(&sealed)?;
    let gz = crucible_crypto::open(&keys, &sealed)
        .map_err(|_| anyhow!("the image is not sealed to the platform key"))?;
    drop(keys);
    let archive = crate::image_archive::Archive::parse(crate::image_archive::gunzip(&gz)?)?;
    drop(gz);
    if outcome["image"]["id"] != json!(archive.id) {
        bail!("the image is not the one the build reported");
    }
    let image_id = archive.id.clone();
    drop(archive);
    let sha256 = s.store(&a.store, true)?.put(&sealed).await?;
    pinned.image = Some(crucible_core::plugins::PluginImage {
        blob: crucible_core::BlobRef { sha256, key_id },
        id: image_id.clone(),
    });
    pinned.check().map_err(|e| anyhow!("{e}"))?;
    eprintln!("plugin {id}: image {image_id} stored");
    let review: Value = match std::fs::read(h.join("review.sealed")) {
        Ok(r) => {
            let keys = s.keys(Secret::PlatformKey)?;
            serde_json::from_slice(&crucible_crypto::open(&keys, &r)?)?
        }
        Err(_) => Value::Null,
    };
    let mut body = json!({
        "status": "ready",
        "plugin": pinned,
        "title": info["title"],
        "description": info["description"],
        "selftest": outcome["selftest"],
    });
    if !review.is_null() {
        body["review"] = review;
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "u-0123456789abcdef";

    fn setup(outcome: &Value, to: &crucible_crypto::PublicKey) -> (tempfile::TempDir, ReportArgs) {
        let d = tempfile::tempdir().unwrap();
        let (h, o) = (d.path().join("h"), d.path().join("o"));
        std::fs::create_dir_all(&h).unwrap();
        std::fs::create_dir_all(&o).unwrap();
        let m = crucible_core::plugins::PluginManifest::parse(
            br#"{"schema":1,"kind":"scorer","name":"kw","version":"1","runs_taskset_code":false}"#,
        )
        .unwrap();
        let pinned = m.pin(
            ID,
            crucible_core::BlobRef {
                sha256: "a".repeat(64),
                key_id: "k1".into(),
            },
        );
        std::fs::write(
            h.join("plugin.json"),
            json!({"plugin": pinned, "title": "kw", "description": ""}).to_string(),
        )
        .unwrap();
        std::fs::write(
            o.join("outcome.sealed"),
            crucible_crypto::seal(to, outcome.to_string().as_bytes()).unwrap(),
        )
        .unwrap();
        let a = ReportArgs {
            common: Default::default(),
            plugin_id: ID.into(),
            worker_url: "https://w.example".into(),
            handoff: Some(h),
            outcome: Some(o),
            store: String::new(),
        };
        (d, a)
    }

    fn secrets(k: &crucible_crypto::PrivateKey) -> (tempfile::TempDir, Secrets) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("platform-key"), k.to_secret_string()).unwrap();
        let s = Secrets::load(super::super::spec("plugin-report"), Some(d.path())).unwrap();
        (d, s)
    }

    #[tokio::test]
    async fn forged_outcomes_are_refused() {
        let platform = crucible_crypto::PrivateKey::generate();
        let (_sd, s) = secrets(&platform);
        let good = json!({"status": "failed", "error": "x", "plugin_id": ID, "package": "a".repeat(64)});
        // Sealed to the platform key and about this plugin: accepted.
        let (_d, a) = setup(&good, &platform.public());
        let b = verify(&a, ID, &s).await.unwrap();
        assert_eq!(b["status"], "failed");
        assert_eq!(b["error"], "x");
        // Sealed to another key (written by the build machine itself).
        let other = crucible_crypto::PrivateKey::generate();
        let (_d, a) = setup(&good, &other.public());
        assert!(verify(&a, ID, &s).await.is_err());
        // Another plugin's or package's outcome.
        let mut wrong = good.clone();
        wrong["package"] = json!("b".repeat(64));
        let (_d, a) = setup(&wrong, &platform.public());
        assert!(verify(&a, ID, &s).await.is_err());
        // "ready" without the image it claims.
        let mut ready = good.clone();
        ready["status"] = json!("ready");
        ready["image"] = json!({"id": format!("sha256:{}", "c".repeat(64))});
        let (_d, a) = setup(&ready, &platform.public());
        assert!(verify(&a, ID, &s).await.is_err());
        // The build did not finish.
        let (_d, mut a) = setup(&good, &platform.public());
        a.outcome = None;
        assert!(verify(&a, ID, &s).await.is_err());
    }
}
