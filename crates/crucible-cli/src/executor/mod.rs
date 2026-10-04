//! The execution backend (docs/executors.md §2.3): how a step starts the
//! containers it needs. Steps and runners only use [`Executor`]; Docker
//! ([`docker::DockerExecutor`]) is the first backend, a native Kubernetes
//! one (Pods + NetworkPolicy) would be the next.
//!
//! Deliberately absent: privileged mode, host paths other than the step's
//! own directories, host networking. Steps cannot express them, so no
//! backend has to guard against them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

pub mod docker;

/// What a backend can do; a step checks its needs against this and refuses
/// to start rather than run with weaker isolation.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// A network whose containers reach only the step's own host ports.
    pub sandbox_net: bool,
    pub pids_limit: bool,
}

/// Resource limits of a container (memory without swap).
#[derive(Debug, Clone)]
pub struct Limits {
    pub memory: String,
    pub cpus: String,
    pub pids: u32,
}

#[derive(Debug, Clone)]
pub struct Mount {
    pub src: PathBuf,
    pub dst: String,
    pub read_only: bool,
}

/// A container to start. `env` holds non-secret values only.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    /// Overrides the image's entrypoint: program, then arguments.
    pub entrypoint: Option<Vec<String>>,
    pub env: Vec<(String, String)>,
    /// `uid:gid`, never root.
    pub user: String,
    pub limits: Limits,
    /// The network to join (a [`Sandbox`]'s).
    pub network: String,
    /// Resolver inside the container (`127.0.0.1`: none).
    pub dns: Option<String>,
    pub mounts: Vec<Mount>,
    pub workdir: Option<String>,
    /// Every container of a run carries `crucible.run=<label>`.
    pub label: String,
}

/// The label key every container of a run carries.
pub const RUN_LABEL: &str = "crucible.run";

/// A sandbox network brought up for one step; torn down when dropped.
pub struct Sandbox {
    /// The network containers join.
    pub network: String,
    /// Where the step's own processes (meter, egress proxy) listen and how
    /// containers reach them.
    pub host: String,
    teardown: Option<Box<dyn FnOnce() + Send>>,
}

impl Sandbox {
    pub fn new(network: String, host: String, teardown: Box<dyn FnOnce() + Send>) -> Sandbox {
        Sandbox {
            network,
            host,
            teardown: Some(teardown),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(t) = self.teardown.take() {
            t();
        }
    }
}

/// How a step starts containers.
#[allow(async_fn_in_trait)]
pub trait Executor {
    fn caps(&self) -> Caps;

    /// A sandbox network whose containers reach only `ports` on
    /// [`Sandbox::host`], checked by probes before it is returned; it
    /// carries the run label (see [`Executor::cleanup`]).
    fn sandbox(&self, ports: &[u16], label: &str) -> Result<Sandbox>;

    /// Start a container; returns its id.
    async fn start(&self, c: &ContainerSpec) -> Result<String>;
    /// Wait for it to exit; its exit code.
    async fn wait(&self, id: &str) -> Result<Option<i64>>;
    async fn stop(&self, id: &str, grace: Duration);
    /// Its last `tail` lines of output (both streams) into `to`.
    async fn logs(&self, id: &str, tail: usize, to: &Path) -> Result<()>;
    async fn remove(&self, id: &str);

    /// Remove what the run labelled `crucible.run=<label>` left: its
    /// containers and its sandbox (network and rules). Scoped: other runs
    /// on the same machine are never touched.
    fn cleanup(&self, label: &str);
}
