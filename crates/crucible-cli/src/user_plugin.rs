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
    Ok(m)
}

/// The image tag the scoring job gives an uploaded plugin.
pub fn image_tag(kind: &str, id: &str) -> String {
    format!("crucible-{kind}-{id}:run")
}

/// Build the package at `root` as `tag` (the backend's image reference);
/// the build log's tail is returned on failure.
pub async fn build(exec: &crate::executor::Backend, root: &Path, tag: &str) -> Result<()> {
    let log = tempfile::NamedTempFile::new()?;
    let b = BuildSpec {
        dir: root.to_path_buf(),
        tag: exec.image_ref(tag),
        quiet: true,
        ..Default::default()
    };
    if let Err(e) = exec.build(&b, log.path()).await {
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
}
