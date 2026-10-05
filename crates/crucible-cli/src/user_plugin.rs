//! Uploaded plugins (docs/plugins.md §14): the package format and building
//! its image. A package is a zip with `plugin.json` and a `Dockerfile` at
//! its root (or under its only top-level folder) plus whatever the build
//! needs. Nothing in it runs on the host: the platform only reads
//! `plugin.json`, and builds and runs the image through the step's
//! execution backend, on a machine that holds no platform secret.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use crucible_core::plugins::PluginManifest;

use crate::executor::{BuildSpec, Executor};
use crate::zipdir::{self, ExtractLimits};

/// Limits for unpacking a plugin package.
pub const PACKAGE_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 2_000,
    max_bytes: 100 << 20,
};

/// Unpack a package into `dest`; returns its root and its `plugin.json`.
pub fn unpack(zip: &[u8], dest: &Path) -> Result<(PathBuf, PluginManifest)> {
    zipdir::safe_extract(Cursor::new(zip), dest, PACKAGE_LIMITS)
        .context("the plugin package is not a usable zip")?;
    let root = package_root(dest)?;
    Ok((root.clone(), check_dir(&root)?))
}

fn package_root(dir: &Path) -> Result<PathBuf> {
    if dir.join("plugin.json").is_file() {
        return Ok(dir.to_path_buf());
    }
    let entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| !matches!(e.file_name().to_str(), Some("__MACOSX" | ".DS_Store")))
        .collect();
    if let [only] = entries.as_slice()
        && only.file_type()?.is_dir()
        && only.path().join("plugin.json").is_file()
    {
        return Ok(only.path());
    }
    bail!("no plugin.json at the top of the package (or of its only folder)")
}

/// A package directory: `plugin.json` valid, a `Dockerfile` next to it.
pub fn check_dir(root: &Path) -> Result<PluginManifest> {
    let raw = std::fs::read(root.join("plugin.json")).context("reading plugin.json")?;
    let m = PluginManifest::parse(&raw).map_err(|e| anyhow!("{e}"))?;
    if !root.join("Dockerfile").is_file() {
        bail!("the package has no Dockerfile next to plugin.json");
    }
    let df = std::fs::read_to_string(root.join("Dockerfile"))
        .context("the Dockerfile is not UTF-8 text")?;
    check_dockerfile(&df)?;
    Ok(m)
}

/// Registries base images may come from (pulled by the Docker daemon, not
/// through the egress proxy). An image name without a registry is Docker
/// Hub's.
pub const BASE_REGISTRIES: &[&str] = &["docker.io", "registry-1.docker.io", "ghcr.io"];

/// What the isolated build (docs/plugins.md §14.3) cannot take: base images
/// from other registries or named by a variable, and `ADD` of a URL or a
/// git repository (the daemon would fetch it, bypassing the proxy).
pub fn check_dockerfile(text: &str) -> Result<()> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let mut stages: Vec<String> = Vec::new();
    for line in joined.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let instr = words.next().unwrap_or("").to_ascii_uppercase();
        let args: Vec<&str> = words.collect();
        match instr.as_str() {
            "FROM" => {
                let mut rest = args.iter().filter(|a| !a.starts_with("--"));
                let image = rest.next().copied().unwrap_or("");
                check_base(image, &stages)?;
                if let (Some(k), Some(alias)) = (rest.next(), rest.next())
                    && k.eq_ignore_ascii_case("as")
                {
                    stages.push(alias.to_ascii_lowercase());
                }
            }
            "ADD" => {
                for a in args.iter().filter(|a| !a.starts_with("--")) {
                    let a = a.trim_matches(|c| c == '"' || c == '[' || c == ']' || c == ',');
                    if a.contains("://") || a.starts_with("git@") {
                        bail!("Dockerfile: ADD of a URL or git repository is not allowed (download it in a RUN step, through the build proxy)");
                    }
                }
            }
            "ONBUILD" => bail!("Dockerfile: ONBUILD is not allowed"),
            _ => {}
        }
    }
    Ok(())
}

fn check_base(image: &str, stages: &[String]) -> Result<()> {
    if image.is_empty() {
        bail!("Dockerfile: FROM without an image");
    }
    if image.contains('$') {
        bail!("Dockerfile: FROM {image}: base images must be named literally");
    }
    let lower = image.to_ascii_lowercase();
    if lower == "scratch" || stages.contains(&lower) {
        return Ok(());
    }
    let first = image.split('/').next().unwrap_or("");
    let has_registry =
        image.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    if has_registry && !BASE_REGISTRIES.contains(&first) {
        bail!(
            "Dockerfile: FROM {image}: base images may come only from Docker Hub or ghcr.io"
        );
    }
    Ok(())
}

/// What an administrator reads before making a plugin public: every file
/// (path, size), the Dockerfile and the text files' contents, within
/// `budget` bytes of text (the rest are listed only).
pub fn review_material(root: &Path, budget: usize) -> Result<serde_json::Value> {
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort();
    let dockerfile = std::fs::read_to_string(root.join("Dockerfile")).unwrap_or_default();
    let dockerfile: String = dockerfile.chars().take(16 * 1024).collect();
    let mut left = budget.saturating_sub(dockerfile.len());
    let mut texts = Vec::new();
    let mut listed = Vec::new();
    // plugin.json first, then the rest by path.
    files.sort_by_key(|(p, _)| (p != "plugin.json", p.clone()));
    // The Worker takes paths of 1-300 characters without control
    // characters, at most 2000 of them.
    files.retain(|(p, _)| p.chars().count() <= 300 && !p.chars().any(char::is_control));
    files.truncate(2000);
    for (path, size) in &files {
        listed.push(serde_json::json!({"path": path, "size": size}));
        if path == "Dockerfile" || *size as usize > left || *size > 16 * 1024 {
            continue;
        }
        if let Ok(t) = std::fs::read_to_string(root.join(path))
            && !t.contains('\0')
        {
            left -= t.len();
            texts.push(serde_json::json!({"path": path, "content": t}));
        }
    }
    Ok(serde_json::json!({"files": listed, "dockerfile": dockerfile, "texts": texts}))
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            walk(root, &e.path(), out)?;
        } else if t.is_file() {
            let rel = e.path().strip_prefix(root)?.to_string_lossy().into_owned();
            out.push((rel, e.metadata()?.len()));
        }
    }
    Ok(())
}

/// The image tag the scoring job gives an uploaded plugin.
pub fn image_tag(kind: &str, id: &str) -> String {
    format!("crucible-{kind}-{id}:run")
}

/// How a registration build is confined.
#[derive(Default)]
pub struct BuildLimits {
    /// RUN steps join this network (the sandbox: only the egress proxy).
    pub network: Option<String>,
    /// `KEY=VALUE` build args (the proxy variables).
    pub build_args: Vec<String>,
    /// Docker's classic-builder limits (`--memory`, ...).
    pub limits: Vec<(String, String)>,
    pub timeout: Option<std::time::Duration>,
}

/// Build the package at `root` as `tag` (the backend's image reference);
/// the build log's tail is returned on failure.
pub async fn build(
    exec: &crate::executor::Backend,
    root: &Path,
    tag: &str,
    lim: BuildLimits,
) -> Result<()> {
    let log = tempfile::NamedTempFile::new()?;
    let b = BuildSpec {
        dir: root.to_path_buf(),
        tag: exec.image_ref(tag),
        quiet: true,
        network: lim.network,
        build_args: lim.build_args,
        limits: lim.limits,
        ..Default::default()
    };
    let r = match lim.timeout {
        Some(t) => match tokio::time::timeout(t, exec.build(&b, log.path())).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("the build took longer than {} minutes", t.as_secs() / 60)),
        },
        None => exec.build(&b, log.path()).await,
    };
    if let Err(e) = r {
        let out = std::fs::read(log.path()).unwrap_or_default();
        let tail = String::from_utf8_lossy(&out[out.len().saturating_sub(2000)..]).into_owned();
        bail!("image build failed: {e:#}\n{tail}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zip(files: &[(&str, &str)]) -> Vec<u8> {
        let d = tempfile::tempdir().unwrap();
        for (n, c) in files {
            let p = d.path().join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, c).unwrap();
        }
        let mut v = Vec::new();
        let mut st = zipdir::ZipStats::default();
        zipdir::collect(d.path(), "", &|_| false, &mut v, &mut st).unwrap();
        zipdir::write_zip(Cursor::new(Vec::new()), &v, &[])
            .unwrap()
            .into_inner()
    }

    const PJ: &str =
        r#"{"schema":1,"kind":"scorer","name":"kw","version":"1","runs_taskset_code":false}"#;

    #[test]
    fn package_layouts() {
        for prefix in ["", "kw/"] {
            let z = zip(&[
                (&format!("{prefix}plugin.json"), PJ),
                (&format!("{prefix}Dockerfile"), "FROM scratch"),
            ]);
            let d = tempfile::tempdir().unwrap();
            let (root, m) = unpack(&z, d.path()).unwrap();
            assert_eq!(m.name, "kw");
            assert!(root.join("Dockerfile").is_file());
        }
        let d = tempfile::tempdir().unwrap();
        assert!(unpack(&zip(&[("plugin.json", PJ)]), d.path()).is_err());
        let d = tempfile::tempdir().unwrap();
        assert!(unpack(&zip(&[("Dockerfile", "FROM scratch")]), d.path()).is_err());
        let d = tempfile::tempdir().unwrap();
        assert!(unpack(b"not a zip", d.path()).is_err());
    }

    #[test]
    fn dockerfile_rules() {
        for ok in [
            "FROM python:3.12-slim@sha256:abc\nCOPY a /a",
            "FROM ghcr.io/x/y:1 AS b\nFROM b\nRUN pip install x",
            "FROM --platform=linux/amd64 docker.io/library/node:22\nADD a.tar /x",
            "FROM scratch",
        ] {
            check_dockerfile(ok).unwrap();
        }
        for bad in [
            "FROM quay.io/x/y",
            "FROM $BASE",
            "FROM python\nADD https://example.com/x /x",
            "FROM python\nADD \\\n  https://example.com/x /x",
            "FROM python\nonbuild RUN x",
        ] {
            assert!(check_dockerfile(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn review_lists_files_and_texts() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("plugin.json"), PJ).unwrap();
        std::fs::write(d.path().join("Dockerfile"), "FROM scratch").unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/a.py"), "print(1)").unwrap();
        std::fs::write(d.path().join("big.bin"), vec![0u8; 100]).unwrap();
        let r = review_material(d.path(), 48 * 1024).unwrap();
        assert_eq!(r["files"].as_array().unwrap().len(), 4);
        assert_eq!(r["files"][0]["path"], "plugin.json");
        assert_eq!(r["dockerfile"], "FROM scratch");
        let texts: Vec<&str> = r["texts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["path"].as_str().unwrap())
            .collect();
        assert_eq!(texts, ["plugin.json", "src/a.py"]);
    }
}
