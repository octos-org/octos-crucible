//! The execution backend (docs/executors.md §2.3): how a step starts the
//! containers it needs. Steps, runners and the plugin scripts (through
//! `crucible ctr`) only use [`Executor`]: Docker ([`docker::DockerExecutor`])
//! or native Kubernetes ([`k8s::K8sExecutor`], Pods + NetworkPolicy).
//!
//! Deliberately absent: privileged mode, host paths other than the step's
//! own directories, host networking. Steps cannot express them, so no
//! backend has to guard against them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

pub mod docker;
pub mod k8s;

/// What a backend can do; a step checks its needs against this and refuses
/// to start rather than run with weaker isolation.
#[derive(Debug, Clone, Copy)]
pub struct Caps {
    /// A network whose containers reach only the step's own host ports.
    pub sandbox_net: bool,
    pub pids_limit: bool,
}

/// Resource limits of a container (memory without swap when both are set).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Limits {
    pub memory: Option<String>,
    pub memory_swap: Option<String>,
    pub cpus: Option<String>,
    pub pids: Option<u32>,
}

/// Where a mount comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum MountSrc {
    /// A directory or file of the step (absolute path).
    Host(PathBuf),
    /// A volume made with [`Executor::volume_create`].
    Volume(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub src: MountSrc,
    pub dst: String,
    pub read_only: bool,
}

impl Mount {
    pub fn host(src: &Path, dst: &str, read_only: bool) -> Mount {
        Mount {
            src: MountSrc::Host(src.to_path_buf()),
            dst: dst.into(),
            read_only,
        }
    }
}

/// An in-memory file system.
#[derive(Debug, Clone, PartialEq)]
pub struct Tmpfs {
    pub dst: String,
    /// Docker's tmpfs options (`rw,noexec,size=64m`, `mode=1777`, ...).
    pub opts: String,
    /// Given as `--mount type=tmpfs` (Docker's defaults differ from `--tmpfs`).
    pub via_mount: bool,
}

/// The network a container joins.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Network {
    /// The backend's default network (Docker: `bridge`, with internet).
    #[default]
    Default,
    /// Loopback only.
    None,
    /// A network made with [`Executor::net_create`] or a [`Sandbox`]'s.
    Named { name: String, aliases: Vec<String> },
    /// The network namespace of another container of the step.
    Container(String),
}

/// A container to start. `env` holds non-secret values only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    /// Overrides the image's entrypoint: program, then arguments.
    pub entrypoint: Option<Vec<String>>,
    /// Arguments after the image (after the entrypoint's own).
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// `uid:gid`; `None`: the image's user.
    pub user: Option<String>,
    pub limits: Limits,
    pub network: Network,
    /// Resolver inside the container (`127.0.0.1`: none).
    pub dns: Option<String>,
    pub mounts: Vec<Mount>,
    pub tmpfs: Vec<Tmpfs>,
    pub workdir: Option<String>,
    /// `key=value` labels; every container of a run carries
    /// `crucible.run=<label>` ([`RUN_LABEL`]).
    pub labels: Vec<(String, String)>,
    /// A minimal init as PID 1 (reaps zombies).
    pub init: bool,
    pub cap_drop: Vec<String>,
    pub cap_add: Vec<String>,
    pub no_new_privileges: bool,
    pub read_only: bool,
    pub shm_size: Option<String>,
    pub platform: Option<String>,
    /// Docker log driver options (other backends ignore them).
    pub log_opts: Vec<String>,
}

/// An image build.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuildSpec {
    pub dir: PathBuf,
    pub tag: String,
    /// No network during the build (RUN steps).
    pub no_network: bool,
    /// RUN steps join this network (a [`Sandbox`]'s: only the step's own
    /// egress proxy is reachable). Docker's classic builder (BuildKit does
    /// not take a named network); base images are pulled by the daemon.
    pub network: Option<String>,
    pub platform: Option<String>,
    pub build_args: Vec<String>,
    pub labels: Vec<(String, String)>,
    /// Docker's legacy-builder limits (`--memory`, `--memory-swap`,
    /// `--cpu-quota`, `--cpu-period`), passed as given.
    pub limits: Vec<(String, String)>,
    /// Print only the image id.
    pub quiet: bool,
    /// `--progress=plain`.
    pub plain_progress: bool,
}

/// An internal network: its containers reach each other and nothing else.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NetSpec {
    pub name: String,
    pub internal: bool,
    pub labels: Vec<(String, String)>,
}

/// What [`Executor::inspect`] reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct State {
    pub running: bool,
    pub exit_code: Option<i64>,
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
///
/// The `*_attached` operations and [`Executor::print_logs`] write to this
/// process's stdout/stderr and return the exit status; with Docker they
/// replace this process with the `docker` command (`exec`), so signals,
/// output and exit status are exactly docker's (`timeout crucible ctr run
/// ...` behaves as `timeout docker run ...`).
#[allow(async_fn_in_trait)]
pub trait Executor {
    fn caps(&self) -> Caps;

    /// A sandbox network whose containers reach only `ports` on
    /// [`Sandbox::host`], checked by probes before it is returned; it
    /// carries the run label (see [`Executor::cleanup`]).
    fn sandbox(&self, ports: &[u16], label: &str) -> Result<Sandbox>;

    /// The reference containers use for an image built or tagged `name`.
    fn image_ref(&self, name: &str) -> String {
        name.to_owned()
    }

    /// Start a container in the background; returns its id.
    async fn start(&self, c: &ContainerSpec) -> Result<String>;
    /// Run a container in the foreground (`rm`: removed when it exits).
    async fn run_attached(&self, c: &ContainerSpec, rm: bool) -> Result<i32>;
    /// Wait for it to exit; its exit code.
    async fn wait(&self, id: &str) -> Result<Option<i64>>;
    async fn stop(&self, id: &str, grace: Duration);
    async fn inspect(&self, id: &str) -> Result<State>;
    /// Its last `tail` lines of output (both streams) into `to`.
    async fn logs(&self, id: &str, tail: usize, to: &Path) -> Result<()>;
    /// Its output (all, or the last `tail` lines) on stdout.
    async fn print_logs(&self, id: &str, tail: Option<usize>) -> Result<i32>;
    async fn remove(&self, id: &str);

    /// Build an image; its output goes to `log`.
    async fn build(&self, b: &BuildSpec, log: &Path) -> Result<()>;
    async fn build_attached(&self, b: &BuildSpec) -> Result<i32>;
    async fn image_exists(&self, image: &str) -> bool;
    async fn image_pull(&self, image: &str) -> Result<()>;
    async fn image_rm(&self, image: &str);
    /// Write `image` as a `docker save` archive to `to`.
    async fn image_save(&self, image: &str, to: &Path) -> Result<()>;
    /// Make the `docker save` archive `from` (checked by
    /// [`crate::image_archive::Archive`]) available as `tag`.
    async fn image_load(&self, from: &Path, tag: &str) -> Result<()>;
    /// Clear the build cache (throwaway machines).
    async fn prune_build_cache(&self);

    /// An internal network (only its own members reach each other).
    async fn net_create(&self, n: &NetSpec) -> Result<()>;
    /// Add a running container to a network under `aliases`.
    async fn net_connect(&self, net: &str, id: &str, aliases: &[String]) -> Result<()>;
    async fn net_rm(&self, net: &str);

    async fn volume_create(&self, name: &str, labels: &[(String, String)]) -> Result<()>;
    async fn volume_rm(&self, name: &str);

    /// Remove what the run labelled `crucible.run=<label>` left: its
    /// containers and its sandbox (network and rules). Scoped: other runs
    /// on the same machine are never touched.
    fn cleanup(&self, label: &str);
}

/// The backend this process uses: `CRUCIBLE_EXECUTOR` (`docker`, the
/// default).
pub enum Backend {
    Docker(docker::DockerExecutor),
    /// `CRUCIBLE_EXECUTOR=k8s`: inside a step Pod (docs/kubernetes.md).
    K8s(Box<k8s::K8sExecutor>),
}

pub fn backend() -> Result<Backend> {
    match std::env::var("CRUCIBLE_EXECUTOR").as_deref() {
        Err(_) | Ok("") | Ok("docker") => Ok(Backend::Docker(docker::DockerExecutor)),
        Ok("k8s") => Ok(Backend::K8s(Box::new(k8s::K8sExecutor::from_env()?))),
        Ok(other) => bail!("CRUCIBLE_EXECUTOR={other}: unknown backend (docker, k8s)"),
    }
}

macro_rules! each {
    ($self:ident, $e:ident => $body:expr) => {
        match $self {
            Backend::Docker($e) => $body,
            Backend::K8s($e) => $body,
        }
    };
}

impl Executor for Backend {
    fn caps(&self) -> Caps {
        each!(self, e => e.caps())
    }
    fn sandbox(&self, ports: &[u16], label: &str) -> Result<Sandbox> {
        each!(self, e => e.sandbox(ports, label))
    }
    fn image_ref(&self, name: &str) -> String {
        each!(self, e => e.image_ref(name))
    }
    async fn start(&self, c: &ContainerSpec) -> Result<String> {
        each!(self, e => e.start(c).await)
    }
    async fn run_attached(&self, c: &ContainerSpec, rm: bool) -> Result<i32> {
        each!(self, e => e.run_attached(c, rm).await)
    }
    async fn wait(&self, id: &str) -> Result<Option<i64>> {
        each!(self, e => e.wait(id).await)
    }
    async fn stop(&self, id: &str, grace: Duration) {
        each!(self, e => e.stop(id, grace).await)
    }
    async fn inspect(&self, id: &str) -> Result<State> {
        each!(self, e => e.inspect(id).await)
    }
    async fn logs(&self, id: &str, tail: usize, to: &Path) -> Result<()> {
        each!(self, e => e.logs(id, tail, to).await)
    }
    async fn print_logs(&self, id: &str, tail: Option<usize>) -> Result<i32> {
        each!(self, e => e.print_logs(id, tail).await)
    }
    async fn remove(&self, id: &str) {
        each!(self, e => e.remove(id).await)
    }
    async fn build(&self, b: &BuildSpec, log: &Path) -> Result<()> {
        each!(self, e => e.build(b, log).await)
    }
    async fn build_attached(&self, b: &BuildSpec) -> Result<i32> {
        each!(self, e => e.build_attached(b).await)
    }
    async fn image_exists(&self, image: &str) -> bool {
        each!(self, e => e.image_exists(image).await)
    }
    async fn image_pull(&self, image: &str) -> Result<()> {
        each!(self, e => e.image_pull(image).await)
    }
    async fn image_rm(&self, image: &str) {
        each!(self, e => e.image_rm(image).await)
    }
    async fn image_save(&self, image: &str, to: &Path) -> Result<()> {
        each!(self, e => e.image_save(image, to).await)
    }
    async fn image_load(&self, from: &Path, tag: &str) -> Result<()> {
        each!(self, e => e.image_load(from, tag).await)
    }
    async fn prune_build_cache(&self) {
        each!(self, e => e.prune_build_cache().await)
    }
    async fn net_create(&self, n: &NetSpec) -> Result<()> {
        each!(self, e => e.net_create(n).await)
    }
    async fn net_connect(&self, net: &str, id: &str, aliases: &[String]) -> Result<()> {
        each!(self, e => e.net_connect(net, id, aliases).await)
    }
    async fn net_rm(&self, net: &str) {
        each!(self, e => e.net_rm(net).await)
    }
    async fn volume_create(&self, name: &str, labels: &[(String, String)]) -> Result<()> {
        each!(self, e => e.volume_create(name, labels).await)
    }
    async fn volume_rm(&self, name: &str) {
        each!(self, e => e.volume_rm(name).await)
    }
    fn cleanup(&self, label: &str) {
        each!(self, e => e.cleanup(label))
    }
}
