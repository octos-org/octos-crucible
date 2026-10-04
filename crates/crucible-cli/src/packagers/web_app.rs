//! `web-app`: a zip with a Dockerfile at its root, built by the scorer with
//! `--network=none` (so everything the app needs at run time must be in it).
//! - Work dir with its own root `Dockerfile`: zipped as is (minus `.git`).
//! - Otherwise (the ARC-Bench convention): `frontend/` + `backend/`, without
//!   `frontend/node_modules`, plus a generated Dockerfile
//!   (`node:24-bookworm-slim`, `WORKDIR /app/backend`, `PORT=3000`,
//!   `CMD <agent.json app_start_cmd>`).
//!
//! An uploaded output (app mode) must have a root `Dockerfile` or a root
//! `backend/` directory (the ARC-Bench template, which the official-format
//! scorer builds itself).
//!
//! The work dir was written by untrusted code: symlinks are dropped, never
//! followed (a followed link could pack a host file).

use std::io::Cursor;
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::zipdir::{Entry, ZipStats, collect, write_zip};

pub struct WebApp;

pub fn dockerfile(app_start_cmd: &[String]) -> String {
    let cmd = serde_json::to_string(app_start_cmd).expect("strings serialize");
    format!(
        "FROM node:24-bookworm-slim\nWORKDIR /app\nCOPY . .\nENV PORT=3000\nEXPOSE 3000\nWORKDIR /app/backend\nCMD {cmd}\n"
    )
}

fn is_git(rel: &str) -> bool {
    rel == ".git" || rel.ends_with("/.git")
}

fn is_real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

impl super::Packager for WebApp {
    fn package(
        &self,
        src: &Path,
        app_start_cmd: &[String],
        _opts: &Value,
    ) -> Result<(Vec<u8>, ZipStats)> {
        let mut entries: Vec<Entry> = Vec::new();
        let mut stats = ZipStats::default();
        let mut extra: Vec<(&str, &[u8])> = Vec::new();
        let generated;
        let root_df = std::fs::symlink_metadata(src.join("Dockerfile"));
        if root_df.is_ok_and(|m| m.is_file()) {
            collect(src, "", &is_git, &mut entries, &mut stats)?;
        } else {
            if !is_real_dir(&src.join("backend")) {
                bail!("no Dockerfile and no backend/ directory in the work dir");
            }
            let skip = |rel: &str| is_git(rel) || rel == "frontend/node_modules";
            collect(src, "backend", &skip, &mut entries, &mut stats)?;
            if is_real_dir(&src.join("frontend")) {
                collect(src, "frontend", &skip, &mut entries, &mut stats)?;
            }
            generated = dockerfile(app_start_cmd);
            extra.push(("Dockerfile", generated.as_bytes()));
        }
        super::size_ok(&stats)?;
        let zip = write_zip(Cursor::new(Vec::new()), &entries, &extra)?.into_inner();
        Ok((zip, stats))
    }

    fn check_names(&self, names: &[String], _opts: &Value) -> Result<()> {
        if names
            .iter()
            .any(|n| n == "Dockerfile" || n.starts_with("backend/"))
        {
            Ok(())
        } else {
            bail!("the output has neither a Dockerfile nor a backend/ directory at its root")
        }
    }

    fn check_options(&self, opts: &Value) -> Result<()> {
        super::no_options(opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn package(src: &Path, cmd: &[String]) -> Result<(Vec<u8>, ZipStats)> {
        crate::packagers::package("web-app", src, cmd, None)
    }

    fn names(zip: &[u8]) -> Vec<String> {
        let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        (0..a.len())
            .map(|i| a.by_index(i).unwrap().name().to_owned())
            .collect()
    }

    fn read(zip: &[u8], name: &str) -> String {
        let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
        let mut s = String::new();
        a.by_name(name).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn web_app_convention() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        for f in [
            "backend/server.js",
            "backend/node_modules/x/index.js",
            "frontend/dist/index.html",
            "frontend/node_modules/y/index.js",
            ".arc/state.db",
            "notes.md",
        ] {
            std::fs::create_dir_all(w.join(f).parent().unwrap()).unwrap();
            std::fs::write(w.join(f), "x").unwrap();
        }
        std::fs::create_dir_all(w.join("backend/.git")).unwrap();
        std::fs::write(w.join("backend/.git/HEAD"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", w.join("backend/leak")).unwrap();
        let (zip, stats) = package(w, &["npm".into(), "start".into()]).unwrap();
        assert_eq!(
            names(&zip),
            [
                "Dockerfile",
                "backend/node_modules/x/index.js",
                "backend/server.js",
                "frontend/dist/index.html"
            ]
        );
        #[cfg(unix)]
        assert_eq!(stats.dropped_links, 1);
        let df = read(&zip, "Dockerfile");
        assert!(df.contains("CMD [\"npm\",\"start\"]"), "{df}");
        assert!(df.contains("WORKDIR /app/backend"));
    }

    #[test]
    fn web_app_own_dockerfile_and_missing_backend() {
        let d = tempfile::tempdir().unwrap();
        assert!(package(d.path(), &["npm".into()]).is_err());
        std::fs::write(d.path().join("Dockerfile"), "FROM scratch").unwrap();
        std::fs::write(d.path().join("app.js"), "x").unwrap();
        let (zip, _) = package(d.path(), &["npm".into()]).unwrap();
        assert_eq!(names(&zip), ["Dockerfile", "app.js"]);
        assert_eq!(read(&zip, "Dockerfile"), "FROM scratch");
        crate::packagers::check("web-app", &zip, None).unwrap();
        let (files, _) = crate::packagers::package("files", d.path(), &[], None).unwrap();
        std::fs::remove_file(d.path().join("Dockerfile")).unwrap();
        let (no_df, _) = crate::packagers::package("files", d.path(), &[], None).unwrap();
        crate::packagers::check("web-app", &files, None).unwrap();
        assert!(crate::packagers::check("web-app", &no_df, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn backend_symlink_refused() {
        let d = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(target.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(target.path(), d.path().join("backend")).unwrap();
        assert!(package(d.path(), &["npm".into()]).is_err());
    }
}
