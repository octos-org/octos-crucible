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

/// Optional build settings. `cache_scope` turns on the shared GitHub
/// Actions layer cache (`docker buildx`, `type=gha`): the workflow passes it
/// for builtin agents only. Uploaded agents never read or write a shared
/// cache, so one submission cannot poison the layers another one builds on.
#[derive(Debug, Default)]
pub struct BuildOpts {
    /// `KEY=VALUE` build args (e.g. a builtin agent's pinned upstream commit).
    pub build_args: Vec<String>,
    pub cache_scope: Option<String>,
}

/// Environment the buildx `gha` cache backend needs (GitHub Actions only).
const GHA_CACHE_ENV: [&str; 4] = [
    "ACTIONS_CACHE_URL",
    "ACTIONS_RESULTS_URL",
    "ACTIONS_RUNTIME_TOKEN",
    "ACTIONS_CACHE_SERVICE_V2",
];

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
    if let Some(s) = &o.cache_scope {
        let ok = !s.is_empty()
            && s.len() <= 128
            && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c));
        if !ok {
            bail!("--cache-scope must match [a-z0-9][a-z0-9._-]{{0,127}}");
        }
    }
    Ok(())
}

pub fn build_args(pkg: &Path, tag: &str, buildkit: bool, o: &BuildOpts) -> Vec<String> {
    let mut a: Vec<String> = match &o.cache_scope {
        Some(scope) => vec![
            "buildx".into(),
            "build".into(),
            "--load".into(),
            "--cache-from".into(),
            format!("type=gha,scope={scope}"),
            "--cache-to".into(),
            format!("type=gha,mode=max,scope={scope}"),
        ],
        None => vec!["build".into()],
    };
    a.extend(["--label".into(), "crucible.agent=1".into()]);
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
    if opts.cache_scope.is_some() && !buildkit {
        bail!("--cache-scope needs BuildKit");
    }
    let mut cmd = docker();
    if opts.cache_scope.is_some() {
        for k in GHA_CACHE_ENV {
            if let Some(v) = std::env::var_os(k) {
                cmd.env(k, v);
            }
        }
    }
    let status = cmd
        .args(build_args(pkg, tag, buildkit, opts))
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
    use super::*;

    #[test]
    fn args() {
        let a = build_args(
            Path::new("/tmp/pkg"),
            "crucible-agent:run",
            true,
            &BuildOpts::default(),
        );
        assert_eq!(a[0], "build");
        assert_eq!(a.last().unwrap(), "/tmp/pkg");
        assert!(a.contains(&"--".to_string()));
        assert!(
            !a.iter()
                .any(|x| x.contains("secret") || x.contains("cache"))
        );
    }

    #[test]
    fn cached_args() {
        let o = BuildOpts {
            build_args: vec!["AGENT_REF=0123abc".into()],
            cache_scope: Some("builtin-octos-0123abc".into()),
        };
        check_opts(&o).unwrap();
        let a = build_args(Path::new("/tmp/pkg"), "t", true, &o);
        assert_eq!(&a[..3], ["buildx", "build", "--load"]);
        assert!(a.contains(&"type=gha,scope=builtin-octos-0123abc".to_string()));
        assert!(a.contains(&"type=gha,mode=max,scope=builtin-octos-0123abc".to_string()));
        assert!(
            a.windows(2)
                .any(|w| w == ["--build-arg", "AGENT_REF=0123abc"])
        );
        assert_eq!(a.last().unwrap(), "/tmp/pkg");
    }

    #[test]
    fn opts_rejected() {
        for o in [
            BuildOpts {
                build_args: vec!["x=1".into()],
                cache_scope: None,
            },
            BuildOpts {
                build_args: vec!["A=$(id)".into()],
                cache_scope: None,
            },
            BuildOpts {
                build_args: vec!["A".into()],
                cache_scope: None,
            },
            BuildOpts {
                build_args: vec![],
                cache_scope: Some("Bad,scope=x".into()),
            },
            BuildOpts {
                build_args: vec![],
                cache_scope: Some("-x".into()),
            },
        ] {
            assert!(check_opts(&o).is_err(), "{o:?}");
        }
    }
}
