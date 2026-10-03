//! `crucible build`: `docker build` of an agent package. The host only runs
//! docker; the package's RUN steps execute in BuildKit containers. Run this
//! before any key exists on the machine.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const MAX_IMAGE_BYTES: u64 = 12 << 30;

#[derive(Debug, Serialize)]
pub struct BuildFacts {
    pub image: String,
    pub size_bytes: u64,
    /// `/agent-build.json` from the image (copied out, never run), if any.
    pub agent_build: serde_json::Value,
}

/// docker with a minimal environment: whatever else the caller's process
/// holds is not handed down.
pub fn docker() -> Command {
    let mut c = Command::new("docker");
    c.env_clear();
    for k in [
        "PATH",
        "HOME",
        "DOCKER_HOST",
        "DOCKER_CONFIG",
        "DOCKER_CONTEXT",
        "XDG_RUNTIME_DIR",
        "TMPDIR",
    ] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    c
}

pub fn build_args(pkg: &Path, tag: &str, buildkit: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["build".into(), "--label".into(), "crucible.agent=1".into()];
    if buildkit {
        a.push("--progress=plain".into());
    }
    a.extend([
        "-t".into(),
        tag.into(),
        "--".into(),
        pkg.display().to_string(),
    ]);
    a
}

pub fn build(pkg: &Path, tag: &str) -> Result<BuildFacts> {
    crate::agentpkg::validate(pkg)?;
    // DOCKER_BUILDKIT=0 only for local smoke tests on hosts without buildx.
    let buildkit = std::env::var("DOCKER_BUILDKIT").map_or(true, |v| v != "0");
    let status = docker()
        .args(build_args(pkg, tag, buildkit))
        .env("DOCKER_BUILDKIT", if buildkit { "1" } else { "0" })
        // Build output goes to stderr so stdout stays machine-readable.
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .context("running docker build")?;
    if !status.success() {
        bail!("docker build failed ({status})");
    }
    let out = docker()
        .args(["image", "inspect", "-f", "{{.Size}}", tag])
        .output()?;
    let size_bytes: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if size_bytes > MAX_IMAGE_BYTES {
        bail!("agent image is {size_bytes} bytes, more than {MAX_IMAGE_BYTES}");
    }
    Ok(BuildFacts {
        image: tag.into(),
        size_bytes,
        agent_build: agent_build_json(tag),
    })
}

/// `docker create` + `docker cp` (the image is not run), at most 2000 bytes.
fn agent_build_json(tag: &str) -> serde_json::Value {
    let Ok(out) = docker().args(["create", tag]).output() else {
        return serde_json::json!({});
    };
    let cid = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let mut value = serde_json::json!({});
    if out.status.success() && !cid.is_empty() {
        if let Ok(tmp) = tempfile::tempdir() {
            let dst = tmp.path().join("agent-build.json");
            let cp = docker()
                .args([
                    "cp",
                    &format!("{cid}:/agent-build.json"),
                    &dst.display().to_string(),
                ])
                .stderr(Stdio::null())
                .stdout(Stdio::null())
                .status();
            if cp.is_ok_and(|s| s.success())
                && std::fs::symlink_metadata(&dst).is_ok_and(|m| m.is_file() && m.len() <= 2000)
                && let Ok(raw) = std::fs::read(&dst)
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw)
                && v.is_object()
            {
                value = v;
            }
        }
        let _ = docker().args(["rm", &cid]).stdout(Stdio::null()).status();
    }
    value
}

#[cfg(test)]
mod tests {
    #[test]
    fn args() {
        let a = super::build_args(std::path::Path::new("/tmp/pkg"), "crucible-agent:run", true);
        assert_eq!(a.last().unwrap(), "/tmp/pkg");
        assert!(a.contains(&"--".to_string()));
        assert!(!a.iter().any(|x| x.contains("secret")));
    }
}
