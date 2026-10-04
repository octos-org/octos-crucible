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
    /// The source commit the build was pinned to; equal to
    /// `agent_build.commit` (checked).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_commit: Option<String>,
}

/// Optional build settings.
#[derive(Debug, Default)]
pub struct BuildOpts {
    /// `KEY=VALUE` build args (e.g. a builtin agent's pinned source commit).
    pub build_args: Vec<String>,
    /// Fail unless the image's `/agent-build.json` records this commit.
    pub expect_commit: Option<String>,
    /// Write docker's build output here instead of stderr (it is the
    /// package's output: kept off public logs).
    pub log: Option<std::path::PathBuf>,
    /// Kill the build after this long.
    pub timeout: Option<std::time::Duration>,
}

fn check_opts(o: &BuildOpts) -> Result<()> {
    for a in &o.build_args {
        let ok = a.split_once('=').is_some_and(|(k, v)| {
            !k.is_empty()
                && k.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && !v.is_empty()
                && v.len() <= 200
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
        });
        if !ok {
            bail!("--build-arg must be KEY=VALUE ([A-Z0-9_]=[A-Za-z0-9._/-]+)");
        }
    }
    if let Some(c) = &o.expect_commit
        && !is_commit(c)
    {
        bail!("--expect-commit must be a full 40-hex commit sha");
    }
    Ok(())
}

fn is_commit(c: &str) -> bool {
    c.len() == 40
        && c.chars()
            .all(|x| x.is_ascii_digit() || ('a'..='f').contains(&x))
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

pub fn build_args(pkg: &Path, tag: &str, buildkit: bool, o: &BuildOpts) -> Vec<String> {
    let mut a: Vec<String> = vec!["build".into(), "--label".into(), "crucible.agent=1".into()];
    if buildkit {
        a.push("--progress=plain".into());
    }
    for b in &o.build_args {
        a.extend(["--build-arg".into(), b.clone()]);
    }
    a.extend([
        "-t".into(),
        tag.into(),
        "--".into(),
        pkg.display().to_string(),
    ]);
    a
}

pub fn build(pkg: &Path, tag: &str, opts: &BuildOpts) -> Result<BuildFacts> {
    crate::agentpkg::validate(pkg)?;
    check_opts(opts)?;
    // DOCKER_BUILDKIT=0 only for local smoke tests on hosts without buildx.
    let buildkit = std::env::var("DOCKER_BUILDKIT").map_or(true, |v| v != "0");
    let mut cmd = docker();
    cmd.args(build_args(pkg, tag, buildkit, opts))
        .env("DOCKER_BUILDKIT", if buildkit { "1" } else { "0" })
        .stdin(Stdio::null());
    match &opts.log {
        Some(p) => {
            let f = std::fs::File::create(p)?;
            cmd.stdout(f.try_clone()?).stderr(f);
        }
        // Build output goes to stderr so stdout stays machine-readable.
        None => {
            cmd.stdout(Stdio::from(std::io::stderr()));
        }
    }
    let mut child = cmd.spawn().context("running docker build")?;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if opts.timeout.is_some_and(|t| started.elapsed() > t) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("docker build timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
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
    let agent_build = agent_build_json(tag);
    if let Some(want) = &opts.expect_commit {
        let got = agent_build["commit"].as_str().unwrap_or("");
        if got != want {
            bail!(
                "the image was built from commit {got:?}, not the pinned {want} (/agent-build.json)"
            );
        }
    }
    Ok(BuildFacts {
        image: tag.into(),
        size_bytes,
        agent_build,
        expected_commit: opts.expect_commit.clone(),
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
    use super::*;

    #[test]
    fn args() {
        let a = build_args(
            Path::new("/tmp/pkg"),
            "crucible-agent:run",
            true,
            &BuildOpts::default(),
        );
        assert_eq!(a.last().unwrap(), "/tmp/pkg");
        assert!(a.contains(&"--".to_string()));
        assert!(!a.iter().any(|x| x.contains("secret")));
    }

    #[test]
    fn pinned_args() {
        let o = BuildOpts {
            build_args: vec!["AGENT_REF=0123456789abcdef0123456789abcdef01234567".into()],
            expect_commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
            ..Default::default()
        };
        check_opts(&o).unwrap();
        let a = build_args(Path::new("/tmp/pkg"), "t", true, &o);
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--build-arg" && w[1].starts_with("AGENT_REF=0123"))
        );
        assert_eq!(a.last().unwrap(), "/tmp/pkg");
    }

    #[test]
    fn opts_rejected() {
        let arg = |s: &str| BuildOpts {
            build_args: vec![s.into()],
            ..Default::default()
        };
        for o in [arg("x=1"), arg("A=$(id)"), arg("A"), arg("A=")] {
            assert!(check_opts(&o).is_err(), "{o:?}");
        }
        for c in ["main", "0123", &"A".repeat(40)] {
            let o = BuildOpts {
                expect_commit: Some(c.into()),
                ..Default::default()
            };
            assert!(check_opts(&o).is_err(), "{c}");
        }
    }
}
