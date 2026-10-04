//! The Docker backend. Agent containers run with `--init`, `--cap-drop
//! ALL`, `no-new-privileges`, memory without swap, CPU and process limits;
//! plugin containers (`crucible ctr`) with exactly the flags their script
//! gives.
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
//!
//! Internal networks (`net_create`) are `docker network create --internal`
//! on a bridge named after the network ([`bridge_name`]); with
//! `CRUCIBLE_SCORER_FIREWALL=1` (CI machines, passwordless sudo) iptables
//! also drop everything the bridge sends to the host or out of the network.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{
    BuildSpec, Caps, ContainerSpec, Executor, MountSrc, NetSpec, Network, RUN_LABEL, Sandbox, State,
};
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

/// The bridge interface of internal network `net`: `crs` + 12 digits
/// (Linux interface names are at most 15 characters; `sandbox-net.sh
/// clean` knows the prefix).
pub fn bridge_name(net: &str) -> String {
    // FNV-1a, stable across builds.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in net.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("crs{:012}", h % 1_000_000_000_000)
}

fn label_args(a: &mut Vec<String>, labels: &[(String, String)]) {
    for (k, v) in labels {
        a.push("--label".into());
        a.push(format!("{k}={v}"));
    }
}

#[derive(Default)]
pub struct DockerExecutor;

impl DockerExecutor {
    /// `docker run -d` arguments of a container.
    pub fn run_args(c: &ContainerSpec) -> Vec<String> {
        Self::run_args_with(c, true, false)
    }

    /// `docker run` arguments: detached, or attached (`rm`: `--rm`).
    pub fn run_args_with(c: &ContainerSpec, detach: bool, rm: bool) -> Vec<String> {
        let mut a: Vec<String> = vec!["run".into()];
        if detach {
            a.push("-d".into());
        }
        if rm {
            a.push("--rm".into());
        }
        if c.init {
            a.push("--init".into());
        }
        if !c.name.is_empty() {
            a.extend(["--name".into(), c.name.clone()]);
        }
        label_args(&mut a, &c.labels);
        match &c.network {
            Network::Default => {}
            Network::None => a.extend(["--network".into(), "none".into()]),
            Network::Named { name, aliases } => {
                a.extend(["--network".into(), name.clone()]);
                for al in aliases {
                    a.extend(["--network-alias".into(), al.clone()]);
                }
            }
            Network::Container(id) => a.extend(["--network".into(), format!("container:{id}")]),
        }
        if let Some(d) = &c.dns {
            a.extend(["--dns".into(), d.clone()]);
        }
        if let Some(p) = &c.platform {
            a.extend(["--platform".into(), p.clone()]);
        }
        if let Some(u) = &c.user {
            a.extend(["--user".into(), u.clone()]);
        }
        let l = &c.limits;
        for (flag, v) in [
            ("--memory", &l.memory),
            ("--memory-swap", &l.memory_swap),
            ("--cpus", &l.cpus),
        ] {
            if let Some(v) = v {
                a.extend([flag.into(), v.clone()]);
            }
        }
        if let Some(p) = l.pids {
            a.extend(["--pids-limit".into(), p.to_string()]);
        }
        for cap in &c.cap_drop {
            a.extend(["--cap-drop".into(), cap.clone()]);
        }
        for cap in &c.cap_add {
            a.extend(["--cap-add".into(), cap.clone()]);
        }
        if c.no_new_privileges {
            a.extend(["--security-opt".into(), "no-new-privileges".into()]);
        }
        if c.read_only {
            a.push("--read-only".into());
        }
        for t in &c.tmpfs {
            if t.via_mount {
                a.push("--mount".into());
                let mut m = format!("type=tmpfs,destination={}", t.dst);
                if !t.opts.is_empty() {
                    m = format!("{m},{}", t.opts);
                }
                a.push(m);
            } else {
                a.push("--tmpfs".into());
                a.push(if t.opts.is_empty() {
                    t.dst.clone()
                } else {
                    format!("{}:{}", t.dst, t.opts)
                });
            }
        }
        if let Some(s) = &c.shm_size {
            a.extend(["--shm-size".into(), s.clone()]);
        }
        for o in &c.log_opts {
            a.extend(["--log-opt".into(), o.clone()]);
        }
        for m in &c.mounts {
            a.push("--mount".into());
            let ro = if m.read_only { ",readonly" } else { "" };
            a.push(match &m.src {
                MountSrc::Host(p) => {
                    format!("type=bind,source={},target={}{ro}", p.display(), m.dst)
                }
                MountSrc::Volume(v) => format!("type=volume,source={v},target={}{ro}", m.dst),
            });
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
        tail.extend(c.args.iter().cloned());
        a.push("--".into());
        a.push(c.image.clone());
        a.extend(tail);
        a
    }

    /// `docker build` arguments.
    pub fn build_args(b: &BuildSpec) -> Vec<String> {
        let mut a: Vec<String> = vec!["build".into()];
        if b.quiet {
            a.push("-q".into());
        }
        if b.plain_progress {
            a.push("--progress=plain".into());
        }
        if b.no_network {
            a.push("--network=none".into());
        }
        if let Some(p) = &b.platform {
            a.extend(["--platform".into(), p.clone()]);
        }
        for (k, v) in &b.limits {
            a.push(format!("{k}={v}"));
        }
        label_args(&mut a, &b.labels);
        for x in &b.build_args {
            a.extend(["--build-arg".into(), x.clone()]);
        }
        a.extend([
            "-t".into(),
            b.tag.clone(),
            "--".into(),
            b.dir.display().to_string(),
        ]);
        a
    }

    /// `docker network create` arguments of an internal network.
    pub fn net_args(n: &NetSpec) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "network".into(),
            "create".into(),
            "--driver".into(),
            "bridge".into(),
        ];
        if n.internal {
            a.push("--internal".into());
        }
        label_args(&mut a, &n.labels);
        a.extend([
            "-o".into(),
            format!("com.docker.network.bridge.name={}", bridge_name(&n.name)),
            n.name.clone(),
        ]);
        a
    }

    /// The firewall of an internal network's bridge: `sudo -n` iptables
    /// commands (`op` = `-I` or `-D`).
    pub fn firewall_cmds(net: &str, op: &str) -> Vec<Vec<String>> {
        let br = bridge_name(net);
        let r = |bin: &str, rest: &[&str]| -> Vec<String> {
            let mut v = vec!["-n".to_string(), bin.to_string(), op.to_string()];
            v.extend(rest.iter().map(|s| s.to_string()));
            v
        };
        vec![
            r("iptables", &["INPUT", "-i", &br, "-j", "DROP"]),
            r("ip6tables", &["INPUT", "-i", &br, "-j", "DROP"]),
            r(
                "iptables",
                &["DOCKER-USER", "-i", &br, "!", "-o", &br, "-j", "DROP"],
            ),
        ]
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

fn firewall_on() -> bool {
    std::env::var("CRUCIBLE_SCORER_FIREWALL").as_deref() == Ok("1")
}

/// Become `docker <args>` (same environment, stdio and signals); returns
/// only if docker cannot be started.
fn exec_docker(args: &[String], extra_env: &[(&str, &str)]) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("docker");
    cmd.args(args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let e = cmd.exec();
    bail!("running docker: {e}")
}

/// `DOCKER_BUILDKIT` unless the caller set it: BuildKit whenever buildx
/// is installed (it cancels the build when the client dies; the legacy
/// builder does not).
fn buildkit_env() -> Option<&'static str> {
    if std::env::var_os("DOCKER_BUILDKIT").is_some() {
        return None;
    }
    let buildx = docker()
        .args(["buildx", "version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    Some(if buildx { "1" } else { "0" })
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

async fn docker_ok(args: &[String]) -> bool {
    tokio::process::Command::from(docker())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
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

fn strs(a: &[String]) -> Vec<&str> {
    a.iter().map(String::as_str).collect()
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
        docker_out(&strs(&Self::run_args(c))).await
    }

    async fn run_attached(&self, c: &ContainerSpec, rm: bool) -> Result<i32> {
        exec_docker(&Self::run_args_with(c, false, rm), &[])
    }

    async fn wait(&self, id: &str) -> Result<Option<i64>> {
        Ok(docker_out(&["wait", id]).await?.parse().ok())
    }

    async fn stop(&self, id: &str, grace: Duration) {
        let _ = docker_out(&["stop", "-t", &grace.as_secs().to_string(), id]).await;
    }

    async fn inspect(&self, id: &str) -> Result<State> {
        let out = docker_out(&[
            "inspect",
            "-f",
            "{{.State.Running}} {{.State.ExitCode}}",
            id,
        ])
        .await?;
        let mut it = out.split_whitespace();
        Ok(State {
            running: it.next() == Some("true"),
            exit_code: it.next().and_then(|c| c.parse().ok()),
        })
    }

    async fn logs(&self, id: &str, tail: usize, to: &Path) -> Result<()> {
        let mut cmd = docker();
        cmd.args(["logs", "--tail", &tail.to_string(), id]);
        write_log(cmd, to).await
    }

    async fn print_logs(&self, id: &str, tail: Option<usize>) -> Result<i32> {
        let mut a = vec!["logs".to_string()];
        if let Some(t) = tail {
            a.extend(["--tail".into(), t.to_string()]);
        }
        a.push(id.into());
        exec_docker(&a, &[])
    }

    async fn remove(&self, id: &str) {
        let _ = docker_out(&["rm", "-f", id]).await;
    }

    async fn build(&self, b: &BuildSpec, log: &Path) -> Result<()> {
        let f = std::fs::File::create(log)?;
        let mut cmd = Command::new("docker");
        if let Some(v) = buildkit_env() {
            cmd.env("DOCKER_BUILDKIT", v);
        }
        let st = tokio::process::Command::from(cmd)
            .args(Self::build_args(b))
            .stdin(Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f)
            .status()
            .await?;
        if !st.success() {
            bail!("docker build of {} failed ({st})", b.tag);
        }
        Ok(())
    }

    async fn build_attached(&self, b: &BuildSpec) -> Result<i32> {
        let env: Vec<(&str, &str)> = buildkit_env()
            .map(|v| ("DOCKER_BUILDKIT", v))
            .into_iter()
            .collect();
        exec_docker(&Self::build_args(b), &env)
    }

    async fn image_exists(&self, image: &str) -> bool {
        docker_ok(&["image".into(), "inspect".into(), image.into()]).await
    }

    async fn image_pull(&self, image: &str) -> Result<()> {
        docker_out(&["pull", "-q", image]).await.map(|_| ())
    }

    async fn image_rm(&self, image: &str) {
        let _ = docker_out(&["image", "rm", "-f", image]).await;
    }

    async fn prune_build_cache(&self) {
        let _ = docker_out(&["builder", "prune", "-f", "--filter", "until=0s"]).await;
    }

    async fn net_create(&self, n: &NetSpec) -> Result<()> {
        docker_out(&strs(&Self::net_args(n))).await?;
        if n.internal && firewall_on() {
            let mut failed = false;
            for c in Self::firewall_cmds(&n.name, "-I") {
                let ok = tokio::process::Command::new("sudo")
                    .args(&c)
                    .stdin(Stdio::null())
                    .status()
                    .await
                    .is_ok_and(|s| s.success());
                failed |= !ok;
            }
            if failed {
                self.net_rm(&n.name).await;
                bail!("could not install the firewall of network {}", n.name);
            }
        }
        Ok(())
    }

    async fn net_connect(&self, net: &str, id: &str, aliases: &[String]) -> Result<()> {
        let mut a = vec!["network".to_string(), "connect".into()];
        for al in aliases {
            a.extend(["--alias".into(), al.clone()]);
        }
        a.extend([net.into(), id.into()]);
        docker_out(&strs(&a)).await.map(|_| ())
    }

    async fn net_rm(&self, net: &str) {
        if firewall_on() {
            for c in Self::firewall_cmds(net, "-D") {
                let _ = tokio::process::Command::new("sudo")
                    .args(&c)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
            }
        }
        let _ = docker_out(&["network", "rm", net]).await;
    }

    async fn volume_create(&self, name: &str, labels: &[(String, String)]) -> Result<()> {
        let mut a = vec!["volume".to_string(), "create".into()];
        label_args(&mut a, labels);
        a.push(name.into());
        docker_out(&strs(&a)).await.map(|_| ())
    }

    async fn volume_rm(&self, name: &str) {
        let _ = docker_out(&["volume", "rm", "-f", name]).await;
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

    #[test]
    fn internal_networks_get_a_firewalled_bridge() {
        let b = bridge_name("crucible-net-17000000001234");
        assert_eq!(b.len(), 15);
        assert!(b.starts_with("crs") && b[3..].bytes().all(|c| c.is_ascii_digit()));
        assert_ne!(b, bridge_name("crucible-net-17000000001235"));
        let a = DockerExecutor::net_args(&NetSpec {
            name: "n1".into(),
            internal: true,
            labels: vec![("crucible.scorer.run".into(), "7".into())],
        });
        assert_eq!(
            a,
            [
                "network",
                "create",
                "--driver",
                "bridge",
                "--internal",
                "--label",
                "crucible.scorer.run=7",
                "-o",
                &format!("com.docker.network.bridge.name={}", bridge_name("n1")),
                "n1"
            ]
        );
        let fw = DockerExecutor::firewall_cmds("n1", "-I");
        assert_eq!(fw.len(), 3);
        assert_eq!(fw[2][..4], ["-n", "iptables", "-I", "DOCKER-USER"]);
    }
}
