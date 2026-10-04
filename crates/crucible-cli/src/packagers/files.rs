//! `files`: the whole work dir as it is, minus `.git`. Option `require`:
//! files that must be at the root of the output (checked when packaging
//! and when an output is uploaded).

use std::io::Cursor;
use std::path::Path;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::zipdir::{Entry, ZipStats, collect, write_zip};

pub struct Files;

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Options {
    #[serde(default)]
    require: Vec<String>,
}

fn options(v: &Value) -> Result<Options> {
    let o: Options = serde_json::from_value(v.clone())
        .map_err(|e| anyhow::anyhow!("packager_options of files: {e}"))?;
    if o.require.len() > 20
        || o.require.iter().any(|r| {
            r.is_empty()
                || r.len() > 200
                || r.starts_with('/')
                || r.contains('\\')
                || r.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        })
    {
        bail!("packager_options.require: at most 20 relative file paths");
    }
    Ok(o)
}

fn is_git(rel: &str) -> bool {
    rel == ".git" || rel.ends_with("/.git")
}

impl super::Packager for Files {
    fn package(&self, work: &Path, _cmd: &[String], opts: &Value) -> Result<(Vec<u8>, ZipStats)> {
        let o = options(opts)?;
        let mut entries: Vec<Entry> = Vec::new();
        let mut stats = ZipStats::default();
        collect(work, "", &is_git, &mut entries, &mut stats)?;
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        missing(&names, &o)?;
        super::size_ok(&stats)?;
        let zip = write_zip(Cursor::new(Vec::new()), &entries, &[])?.into_inner();
        Ok((zip, stats))
    }

    fn check_names(&self, names: &[String], opts: &Value) -> Result<()> {
        missing(names, &options(opts)?)
    }

    fn check_options(&self, opts: &Value) -> Result<()> {
        options(opts).map(|_| ())
    }
}

fn missing(names: &[String], o: &Options) -> Result<()> {
    if let Some(r) = o.require.iter().find(|r| !names.contains(r)) {
        bail!("the output has no {r} at its root");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::packagers::{check, package};
    use serde_json::json;

    #[test]
    fn require() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        std::fs::write(d.path().join(".git/HEAD"), "x").unwrap();
        std::fs::write(d.path().join("answer.md"), "proof").unwrap();
        let req = json!({"require": ["answer.md"]});
        let (zip, stats) = package("files", d.path(), &[], Some(&req)).unwrap();
        assert_eq!(stats.files, 1);
        check("files", &zip, Some(&req)).unwrap();
        let other = json!({"require": ["observer.project.json"]});
        assert!(package("files", d.path(), &[], Some(&other)).is_err());
        assert!(check("files", &zip, Some(&other)).is_err());
        assert!(check("files", b"not a zip", None).is_err());
        assert!(
            crate::packagers::check_options("files", Some(&json!({"require": ["../x"]}))).is_err()
        );
        assert!(crate::packagers::check_options("files", Some(&json!({"other": 1}))).is_err());
        assert!(crate::packagers::check_options("web-app", Some(&req)).is_err());
    }
}
