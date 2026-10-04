//! The Docker backend. Containers run with `--init`, `--cap-drop ALL`,
//! `no-new-privileges`, memory without swap, CPU and process limits.
//!
//! Sandbox networks are tools/sandbox-net.sh (compiled into this binary,
//! so the script always matches it): a bridge with NAT and inter-container
//! traffic off, iptables rules that drop all forwarded traffic and accept
//! from the bridge only the given TCP ports on its host side, checked by
//! probes before use. Several can exist on one machine: slot `k` is network
//! `crucible-sbx[-k]`, bridge `crucible<k>`, `172.31.(250-k).0/24`. A slot
//! is taken by creating its docker network, which docker does atomically
//! (a name or subnet in use fails), so concurrent steps never share one
//! and nothing needs a lock file. The proxies of each slot listen on its
//! own host address, so the ports stay the same.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{Caps, ContainerSpec, Executor, RUN_LABEL, Sandbox};
use crate::build::docker;

const SCRIPT: &str = include_str!("../../../../tools/sandbox-net.sh");

/// Slots 0..MAX_SLOTS: 172.31.201.0/24 ..= 172.31.250.0/24.
pub const MAX_SLOTS: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub slot: u32,
    pub network: String,
    pub bridge: String,
    pub subnet: String,
    pub gateway: String,
}

impl Slot {
    pub fn new(k: u32) -> Slot {
        let net = 250 - k;
        Slot {
            slot: k,
            network: if k == 0 {
                "crucible-sbx".into()
            } else {
                format!("crucible-sbx-{k}")
            },
            bridge: format!("crucible{k}"),
            subnet: format!("172.31.{net}.0/24"),
            gateway: format!("172.31.{net}.1"),
        }
    }
}

#[derive(Default)]
pub struct DockerExecutor;

impl DockerExecutor {
    /// `docker run` arguments of a container.
    pub fn run_args(c: &ContainerSpec) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--init".into(),
            "--name".into(),
            c.name.clone(),
            "--label".into(),
            format!("{RUN_LABEL}={}", c.label),
            "--network".into(),
            c.network.clone(),
        ];
        if let Some(d) = &c.dns {
            a.extend(["--dns".into(), d.clone()]);
        }
        a.extend([
            "--user".into(),
            c.user.clone(),
            "--memory".into(),
            c.limits.memory.clone(),
            "--memory-swap".into(),
            c.limits.memory.clone(),
            "--cpus".into(),
            c.limits.cpus.clone(),
            "--pids-limit".into(),
            c.limits.pids.to_string(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
        ]);
        for m in &c.mounts {
            a.push("--mount".into());
            a.push(format!(
                "type=bind,source={},target={}{}",
                m.src.display(),
                m.dst,
                if m.read_only { ",readonly" } else { "" }
            ));
        }
        if let Some(w) = &c.workdir {
            a.extend(["--workdir".into(), w.clone()]);
        }
        for (k, v) in &c.env {
            a.push("--env".into());
            a.push(format!("{k}={v}"));
        }
        let mut tail = Vec::new();
        if let Some(ep) = &c.entrypoint {
            a.push("--entrypoint".into());
            a.push(ep[0].clone());
            tail.extend(ep[1..].iter().cloned());
        }
        a.push("--".into());
        a.push(c.image.clone());
        a.extend(tail);
        a
    }

    /// Run tools/sandbox-net.sh `verb` for `slot`; its exit code.
    fn script(slot: &Slot, ports: &str, label: &str, verb: &str) -> Result<Option<i32>> {
        let f = tempfile::NamedTempFile::new()?;
        std::fs::write(f.path(), SCRIPT)?;
        let status = Command::new("bash")
            .arg(f.path())
            .arg(verb)
            .env("SANDBOX_PORTS", ports)
            .env("SANDBOX_NET", &slot.network)
            .env("SANDBOX_BRIDGE", &slot.bridge)
            .env("SANDBOX_SUBNET", &slot.subnet)
            .env("SANDBOX_GW", &slot.gateway)
            .env("SANDBOX_LABEL", label)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("sandbox-net.sh {verb}"))?;
        Ok(status.code())
    }
}

async fn docker_out(args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::from(docker())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        // docker's own message (never holds a secret: none is passed to
        // docker in any form).
        let err = String::from_utf8_lossy(&out.stderr);
        let err: String = err.trim().chars().take(300).collect();
        bail!("docker {} failed: {err}", args.first().unwrap_or(&""));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Run `cmd` (`docker logs`) with its stdout and stderr both going to `path`.
async fn write_log(cmd: Command, path: &Path) -> Result<()> {
    let log = std::fs::File::create(path)?;
    tokio::process::Command::from(cmd)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .await?;
    Ok(())
}

impl Executor for DockerExecutor {
    fn caps(&self) -> Caps {
        Caps {
            sandbox_net: true,
            pids_limit: true,
        }
    }

    fn sandbox(&self, ports: &[u16], label: &str) -> Result<Sandbox> {
        let ports = ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        for k in 0..MAX_SLOTS {
            let slot = Slot::new(k);
            match Self::script(&slot, &ports, label, "up")? {
                Some(0) => {}
                // The network exists (another evaluation's) or its subnet
                // is in use: the next slot.
                Some(3) => continue,
                other => {
                    let _ = Self::script(&slot, &ports, label, "down");
                    bail!("sandbox network up failed (exit {other:?})");
                }
            }
            let (s2, p2, l2) = (slot.clone(), ports.clone(), label.to_owned());
            let sb = Sandbox::new(
                slot.network.clone(),
                slot.gateway.clone(),
                Box::new(move || {
                    let _ = Self::script(&s2, &p2, &l2, "down");
                }),
            );
            if Self::script(&slot, &ports, label, "check")? != Some(0) {
                bail!("sandbox network check failed: refusing to run");
            }
            eprintln!(
                "sandbox slot {k}: {} {} host {}",
                slot.network, slot.subnet, slot.gateway
            );
            return Ok(sb);
        }
        bail!("no free sandbox slot (0..{MAX_SLOTS})")
    }

    async fn start(&self, c: &ContainerSpec) -> Result<String> {
        let a = Self::run_args(c);
        docker_out(&a.iter().map(String::as_str).collect::<Vec<_>>()).await
    }

    async fn wait(&self, id: &str) -> Result<Option<i64>> {
        Ok(docker_out(&["wait", id]).await?.parse().ok())
    }

    async fn stop(&self, id: &str, grace: Duration) {
        let _ = docker_out(&["stop", "-t", &grace.as_secs().to_string(), id]).await;
    }

    async fn logs(&self, id: &str, tail: usize, to: &Path) -> Result<()> {
        let mut cmd = docker();
        cmd.args(["logs", "--tail", &tail.to_string(), id]);
        write_log(cmd, to).await
    }

    async fn remove(&self, id: &str) {
        let _ = docker_out(&["rm", "-f", id]).await;
    }

    fn cleanup(&self, label: &str) {
        let filter = format!("label={RUN_LABEL}={label}");
        let ids = |what: &[&str]| -> Vec<String> {
            docker()
                .args(what)
                .args(["--filter", &filter])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        for id in ids(&["ps", "-aq"]) {
            let _ = docker()
                .args(["rm", "-f", &id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // A sandbox this run left (killed before it could take it down):
        // its slot's network and rules.
        for name in ids(&["network", "ls", "--format", "{{.Name}}"]) {
            if let Some(slot) = (0..MAX_SLOTS).map(Slot::new).find(|s| s.network == name) {
                let _ = Self::script(&slot, "8787,3128", label, "down");
                let _ = Self::script(&slot, "8787", label, "down");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_log_has_stdout_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.log");
        // Stands in for `docker logs`, which replays both streams.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo to-stdout; echo to-stderr >&2"]);
        write_log(cmd, &path).await.unwrap();
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.contains("to-stdout"), "{log}");
        assert!(log.contains("to-stderr"), "{log}");
    }

    #[test]
    fn slots_do_not_overlap_and_slot_0_is_the_classic_one() {
        let s0 = Slot::new(0);
        assert_eq!(
            (
                s0.network.as_str(),
                s0.bridge.as_str(),
                s0.subnet.as_str(),
                s0.gateway.as_str()
            ),
            (
                "crucible-sbx",
                "crucible0",
                "172.31.250.0/24",
                "172.31.250.1"
            )
        );
        let all: std::collections::BTreeSet<String> =
            (0..MAX_SLOTS).map(|k| Slot::new(k).subnet).collect();
        assert_eq!(all.len(), MAX_SLOTS as usize);
        for k in 0..MAX_SLOTS {
            // Linux interface names are at most 15 characters.
            assert!(Slot::new(k).bridge.len() <= 15);
        }
    }
}
