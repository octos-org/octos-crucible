//! The sandbox network of a step (tools/sandbox-net.sh, compiled into this
//! binary so the script always matches it): a docker bridge whose
//! containers reach only the host side's meter / egress proxy ports.
//! Brought up and checked by [`SandboxNet::up`], taken down when dropped.

use std::process::Command;

use anyhow::{Context, Result, bail};

const SCRIPT: &str = include_str!("../../../tools/sandbox-net.sh");

pub struct SandboxNet {
    /// Docker network the containers join.
    pub network: String,
    /// Host side of the bridge: the proxies listen here.
    pub gateway: String,
    ports: String,
    script: tempfile::NamedTempFile,
    up: bool,
}

impl SandboxNet {
    /// Create the network, install its rules and run the isolation probes;
    /// `ports` are the host ports containers may reach (e.g. "8787,3128").
    pub fn up(ports: &str) -> Result<SandboxNet> {
        let script = tempfile::NamedTempFile::new()?;
        std::fs::write(script.path(), SCRIPT)?;
        let mut n = SandboxNet {
            network: "crucible-sbx".into(),
            gateway: "172.31.250.1".into(),
            ports: ports.into(),
            script,
            up: false,
        };
        // `down` is idempotent: whatever `up` got to is undone on failure.
        n.up = true;
        n.run("up")?;
        n.run("check")?;
        Ok(n)
    }

    fn run(&self, verb: &str) -> Result<()> {
        let status = Command::new("bash")
            .arg(self.script.path())
            .arg(verb)
            .env("SANDBOX_PORTS", &self.ports)
            .stdin(std::process::Stdio::null())
            .status()
            .with_context(|| format!("sandbox-net.sh {verb}"))?;
        if !status.success() {
            bail!("sandbox network {verb} failed ({status})");
        }
        Ok(())
    }

    pub fn down(&mut self) {
        if std::mem::take(&mut self.up) {
            let _ = self.run("down");
        }
    }
}

impl Drop for SandboxNet {
    fn drop(&mut self) {
        self.down();
    }
}
