//! The native Kubernetes backend (docs/executors.md §3.2, docs/kubernetes.md):
//! every container is a Pod, networks are NetworkPolicies, images are built
//! by the cluster's BuildKit and kept in the cluster's registry.
//!
//! It runs inside a step Pod (`crucible eval k8s` starts one Job per step)
//! and talks to the API server with `kubectl` (the step's service account
//! may only manage Pods and NetworkPolicies of its evaluation's namespace).
//! Configuration comes from the step Pod's environment ([`Conf`]).
//!
//! - **Files.** The evaluation's PVC is mounted at the same path
//!   (`CRUCIBLE_K8S_ROOT`) in the step Pod and in every Pod it starts; a
//!   host path of a container spec must lie under it and becomes a
//!   `subPath` of that volume, a named volume is the directory
//!   `.volumes/<name>` on it. With a ReadWriteOnce volume (`local-path`)
//!   Pods run on the step's node (required node affinity); with a
//!   ReadWriteMany one (`CRUCIBLE_K8S_SAME_NODE=0`) on any node of the
//!   sandbox pool (`CRUCIBLE_K8S_NODE_SELECTOR`, with matching
//!   tolerations), except Pods with a named volume, which stay on the
//!   step's node (named volumes may carry FIFOs, node-local).
//! - **Networks.** The namespace denies everything by default. A pod on an
//!   internal network carries the label `net.crucible/<net>=1` and the
//!   network's policy lets its members reach each other only; aliases are
//!   `hostAliases` (the pods have no DNS: `dnsPolicy: None`, 127.0.0.1). A
//!   sandbox is such a network whose members may reach only the given TCP
//!   ports of the step Pod (and nothing else, not each other); it is
//!   checked by probes before use and refused if any probe gets through
//!   (a CNI that does not enforce NetworkPolicy fails here). A pod on the
//!   default network (no `--network`) may reach the internet (not private
//!   or link-local addresses) and the cluster DNS.
//! - **Containers.** Docker's flags map to the Pod spec: user, capabilities,
//!   `allowPrivilegeEscalation: false` (no-new-privileges), read-only root,
//!   `RuntimeDefault` seccomp, limits = requests (memory, CPU), tmpfs and
//!   `/dev/shm` as memory `emptyDir`s, `shareProcessNamespace` for
//!   `--init` (the pause process reaps zombies). No service account token,
//!   no service links. The process limit is the node's (kubelet
//!   `pod-max-pids`), declared with `CRUCIBLE_K8S_PIDS_LIMIT=1`. In a
//!   namespace that enforces the `restricted` Pod Security Standard
//!   (`CRUCIBLE_K8S_POD_SECURITY=restricted`) every container is made to
//!   meet it ([`security_context`]); `CRUCIBLE_K8S_RUNTIME_CLASS` puts
//!   every Pod under that RuntimeClass (gVisor, Kata).
//! - **Shared network namespaces.** `--network container:<name>` (a test
//!   reaching the app at 127.0.0.1) becomes an ephemeral container of
//!   `<name>`'s Pod ([`guest_spec`]): same network namespace, same
//!   policies, the host's resources grown by the guest's.
//! - **Images.** `crucible-*` names map to `<registry>/crucible-*`; builds
//!   go to BuildKit (`buildctl`), with `force-network-mode=none` for builds
//!   without network, and are pushed there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{
    BuildSpec, Caps, ContainerSpec, Executor, MountSrc, NetSpec, Network, RUN_LABEL, Sandbox, State,
};

/// busybox 1.37.0, the same probe image as tools/sandbox-net.sh.
pub const PROBE_IMAGE: &str =
    "busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";

/// The label prefix of network membership.
const NET_PREFIX: &str = "net.crucible";
const ALIAS_PREFIX: &str = "alias.crucible";
/// A host Pod's label per guest container ([`guest_label`]).
const GUEST_PREFIX: &str = "guest.crucible";
/// Pods started by a step (not the step itself).
const ROLE: &str = "crucible/role";
/// Pods that may reach the internet.
const INTERNET: &str = "crucible/internet";
/// The step Pod's own label (its Job name).
pub const STEP_LABEL: &str = "crucible/step";
const DATA_VOLUME: &str = "data";

/// Where the backend runs: the step Pod's environment.
#[derive(Debug, Clone, PartialEq)]
pub struct Conf {
    /// `CRUCIBLE_K8S_NAMESPACE`: the evaluation's namespace.
    pub namespace: String,
    /// `CRUCIBLE_K8S_ROOT`: where the evaluation's PVC is mounted.
    pub root: PathBuf,
    /// `CRUCIBLE_K8S_PVC`.
    pub pvc: String,
    /// `CRUCIBLE_K8S_STEP`: this step Pod's `crucible/step` label.
    pub step: String,
    /// `POD_IP`, `NODE_NAME`, `HOST_IP` (downward API).
    pub pod_ip: String,
    pub node: String,
    pub host_ip: String,
    /// `CRUCIBLE_K8S_REGISTRY` (`host:port`), where images are pushed and
    /// pulled from.
    pub registry: String,
    /// `CRUCIBLE_K8S_BUILDKIT` (`tcp://...:1234`).
    pub buildkit: String,
    /// `CRUCIBLE_K8S_DNS`: the cluster DNS service address (a probe).
    pub dns: String,
    /// `CRUCIBLE_K8S_PIDS_LIMIT=1`: the nodes limit processes per Pod.
    pub pids_limit: bool,
    /// `CRUCIBLE_K8S_SAME_NODE` (default 1): Pods on the step's node (a
    /// ReadWriteOnce volume); 0: anywhere in the pool (ReadWriteMany).
    pub same_node: bool,
    /// `CRUCIBLE_K8S_NODE_SELECTOR` (`k=v,...`): the sandbox pool's nodes.
    pub node_selector: Vec<(String, String)>,
    /// `CRUCIBLE_K8S_POD_SECURITY=restricted`: the namespace enforces the
    /// `restricted` Pod Security Standard, so every Pod is made to meet
    /// it ([`restrict`]); anything else keeps the container's own flags.
    pub restricted: bool,
    /// `CRUCIBLE_K8S_RUNTIME_CLASS`: the RuntimeClass of every Pod a step
    /// starts (gVisor `runsc`, Kata); empty: the cluster's default.
    pub runtime_class: Option<String>,
}

impl Conf {
    pub fn from_env() -> Result<Conf> {
        let get = |k: &str| -> Result<String> {
            std::env::var(k)
                .ok()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("{k} is not set (Kubernetes backend, docs/kubernetes.md)"))
        };
        let opt = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
        Ok(Conf {
            namespace: get("CRUCIBLE_K8S_NAMESPACE")?,
            root: PathBuf::from(get("CRUCIBLE_K8S_ROOT")?),
            pvc: get("CRUCIBLE_K8S_PVC")?,
            step: get("CRUCIBLE_K8S_STEP")?,
            pod_ip: get("POD_IP")?,
            node: get("NODE_NAME")?,
            host_ip: opt("HOST_IP", ""),
            registry: get("CRUCIBLE_K8S_REGISTRY")?,
            buildkit: get("CRUCIBLE_K8S_BUILDKIT")?,
            dns: opt("CRUCIBLE_K8S_DNS", "10.43.0.10"),
            pids_limit: opt("CRUCIBLE_K8S_PIDS_LIMIT", "0") == "1",
            same_node: opt("CRUCIBLE_K8S_SAME_NODE", "1") != "0",
            node_selector: parse_selector(&opt("CRUCIBLE_K8S_NODE_SELECTOR", ""))?,
            restricted: opt("CRUCIBLE_K8S_POD_SECURITY", "") == "restricted",
            runtime_class: Some(opt("CRUCIBLE_K8S_RUNTIME_CLASS", "")).filter(|r| !r.is_empty()),
        })
    }
}

/// A node selector `key=value[,key=value]` (empty: none).
pub fn parse_selector(s: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for kv in s.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow!("node selector {kv:?}: key=value"))?;
        let ok = |x: &str| {
            !x.is_empty()
                && x.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
        };
        if !ok(k) || !(v.is_empty() || ok(v)) || v.contains('/') {
            bail!("node selector {kv:?}");
        }
        out.push((k.to_owned(), v.to_owned()));
    }
    Ok(out)
}

/// `selector` as `k=v,...` (the environment form).
pub fn selector_string(selector: &[(String, String)]) -> String {
    selector
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Pin a Pod spec to a pool: `nodeSelector` (merged with what is there)
/// and a toleration per label (`key=value:NoSchedule`, so the pool's nodes
/// may be tainted with their own label to keep other Pods off).
pub fn place(spec: &mut Value, selector: &[(String, String)]) {
    if selector.is_empty() {
        return;
    }
    if !spec["nodeSelector"].is_object() {
        spec["nodeSelector"] = json!({});
    }
    for (k, v) in selector {
        spec["nodeSelector"][k] = json!(v);
    }
    spec["tolerations"] = json!(
        selector
            .iter()
            .map(
                |(k, v)| json!({"key": k, "operator": "Equal", "value": v, "effect": "NoSchedule"})
            )
            .collect::<Vec<_>>()
    );
}

/// A DNS-1123 name (Pods, policies): lowercase, `[a-z0-9-]`, at most 63;
/// longer or changed names keep a hash of the original so they stay unique.
pub fn dns_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() { c } else { '-' }
        })
        .collect();
    out = out.trim_matches('-').to_owned();
    if out != s || out.len() > 63 || out.is_empty() {
        let h = format!("{:010x}", fnv(s) % 0xff_ffff_ffff);
        out.truncate(52);
        out = format!("{}-{h}", out.trim_end_matches('-'));
        out = out.trim_start_matches('-').to_owned();
    }
    out
}

/// A label value (or the name part of a key): `[A-Za-z0-9._-]`, at most
/// 63, alphanumeric at both ends.
pub fn label_value(s: &str) -> String {
    let ok = s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars().last().is_some_and(|c| c.is_ascii_alphanumeric());
    if ok || s.is_empty() {
        return s.to_owned();
    }
    dns_name(s)
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// The membership label of network `net`.
pub fn net_label(net: &str) -> String {
    format!("{NET_PREFIX}/{}", label_value(net))
}

fn alias_label(net: &str) -> String {
    format!("{ALIAS_PREFIX}/{}", label_value(net))
}

/// Docker's sizes (`512m`, `2g`, `1024k`, bytes) as Kubernetes quantities.
pub fn quantity(docker: &str) -> Result<String> {
    let s = docker.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    if num.is_empty() {
        bail!("size {docker:?}");
    }
    Ok(match unit.to_ascii_lowercase().as_str() {
        "" | "b" => num.to_owned(),
        "k" | "kb" => format!("{num}Ki"),
        "m" | "mb" => format!("{num}Mi"),
        "g" | "gb" => format!("{num}Gi"),
        _ => bail!("size {docker:?}"),
    })
}

/// Docker's `--cpus` (`1`, `1.0`, `0.5`) as a CPU quantity.
pub fn cpus(docker: &str) -> Result<String> {
    let v: f64 = docker.parse().map_err(|_| anyhow!("--cpus {docker:?}"))?;
    if v <= 0.0 {
        bail!("--cpus {docker:?}");
    }
    Ok(format!("{}m", (v * 1000.0).round() as u64))
}

/// The `size=` (or `tmpfs-size=`) of tmpfs options, as a quantity.
fn tmpfs_size(opts: &str) -> Result<Option<String>> {
    for o in opts.split(',') {
        if let Some(v) = o
            .strip_prefix("size=")
            .or_else(|| o.strip_prefix("tmpfs-size="))
        {
            return quantity(v).map(Some);
        }
    }
    Ok(None)
}

/// `uid:gid` (numeric).
fn user(u: &str) -> Result<(i64, Option<i64>)> {
    let (a, b) = match u.split_once(':') {
        Some((a, b)) => (a, Some(b)),
        None => (u, None),
    };
    let n = |x: &str| {
        x.parse::<i64>()
            .map_err(|_| anyhow!("--user must be numeric uid[:gid] on Kubernetes"))
    };
    Ok((n(a)?, b.map(n).transpose()?))
}

/// The image reference Pods use: our own images (`crucible-*`) are in the
/// cluster registry; any other reference is pulled as given.
pub fn map_image(registry: &str, name: &str) -> String {
    if name.starts_with("crucible-") && !registry.is_empty() {
        format!("{}/{name}", registry.trim_end_matches('/'))
    } else {
        name.to_owned()
    }
}

/// The uid a container runs as in a `restricted` namespace when it asks
/// for root or for the image's user (the steps' own uid).
pub const RESTRICTED_UID: i64 = 1000;

/// A container's security context from Docker's flags. In a `restricted`
/// namespace it is made to meet that Pod Security Standard: no privilege
/// escalation, every capability dropped (only `NET_BIND_SERVICE` may be
/// added back), a non-root user (root or the image's user becomes
/// [`RESTRICTED_UID`]).
pub fn security_context(c: &ContainerSpec, restricted: bool) -> Result<Value> {
    let mut sc = json!({
        "allowPrivilegeEscalation": !c.no_new_privileges,
        "readOnlyRootFilesystem": c.read_only,
        "capabilities": {"drop": c.cap_drop, "add": c.cap_add},
        "seccompProfile": {"type": "RuntimeDefault"},
    });
    let mut ug = c.user.as_deref().map(user).transpose()?;
    if restricted {
        sc["allowPrivilegeEscalation"] = json!(false);
        let add: Vec<&str> = c
            .cap_add
            .iter()
            .filter(|x| {
                x.trim_start_matches("CAP_")
                    .eq_ignore_ascii_case("NET_BIND_SERVICE")
            })
            .map(|_| "NET_BIND_SERVICE")
            .collect();
        sc["capabilities"] = json!({"drop": ["ALL"], "add": add});
        if ug.is_none_or(|(u, _)| u == 0) {
            eprintln!(
                "pod security restricted: {} runs as {RESTRICTED_UID}:{RESTRICTED_UID} (not as {})",
                if c.name.is_empty() { &c.image } else { &c.name },
                c.user.as_deref().unwrap_or("the image's user")
            );
            ug = Some((RESTRICTED_UID, Some(RESTRICTED_UID)));
        }
    }
    if let Some((uid, gid)) = ug {
        sc["runAsUser"] = json!(uid);
        sc["runAsNonRoot"] = json!(uid != 0);
        if let Some(g) = gid {
            sc["runAsGroup"] = json!(g);
        }
    }
    Ok(sc)
}

/// The mounts of `c`'s host paths and named volumes: subPaths of the
/// evaluation's volume.
fn data_mounts(conf: &Conf, c: &ContainerSpec) -> Result<Vec<Value>> {
    let mut mounts = Vec::new();
    for m in &c.mounts {
        let sub = mount_sub(conf, &m.src)?;
        let mut vm = json!({"name": DATA_VOLUME, "mountPath": m.dst, "readOnly": m.read_only});
        if !sub.is_empty() {
            vm["subPath"] = json!(sub);
        }
        mounts.push(vm);
    }
    Ok(mounts)
}

/// Where a mount's source is on the evaluation's volume (relative).
fn mount_sub(conf: &Conf, src: &MountSrc) -> Result<String> {
    Ok(match src {
        MountSrc::Host(p) => {
            let rel = p.strip_prefix(&conf.root).map_err(|_| {
                anyhow!(
                    "{} is not under {} (the evaluation's volume)",
                    p.display(),
                    conf.root.display()
                )
            })?;
            if rel
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                bail!("{}: no `..` in mounted paths", p.display());
            }
            rel.display().to_string()
        }
        MountSrc::Volume(v) => format!(".volumes/{}", volume_dir(v)?),
    })
}

/// The container of `c` (no resources): image, command, environment,
/// security context, `mounts`.
fn container(conf: &Conf, c: &ContainerSpec, name: &str, mounts: Vec<Value>) -> Result<Value> {
    let mut ctr = json!({
        "name": name,
        "image": map_image(&conf.registry, &c.image),
        "imagePullPolicy": "IfNotPresent",
        "env": c.env.iter().map(|(k, v)| json!({"name": k, "value": v})).collect::<Vec<_>>(),
        "securityContext": security_context(c, conf.restricted)?,
        "volumeMounts": mounts,
    });
    if let Some(ep) = &c.entrypoint {
        ctr["command"] = json!(ep);
    }
    if !c.args.is_empty() {
        ctr["args"] = json!(c.args);
    }
    if let Some(w) = &c.workdir {
        ctr["workingDir"] = json!(w);
    }
    Ok(ctr)
}

/// The Pod of a container. `aliases` are the `hostAliases` (alias, IP) of
/// its network's other members.
pub fn pod_spec(conf: &Conf, c: &ContainerSpec, aliases: &[(String, String)]) -> Result<Value> {
    let mut labels = BTreeMap::new();
    labels.insert(ROLE.to_string(), "sandbox".to_string());
    for (k, v) in &c.labels {
        labels.insert(k.clone(), label_value(v));
    }
    let mut dns = json!({"dnsPolicy": "None", "dnsConfig": {"nameservers": [c.dns.clone().unwrap_or_else(|| "127.0.0.1".into())]}});
    match &c.network {
        Network::Default => {
            labels.insert(INTERNET.into(), "1".into());
            dns = json!({"dnsPolicy": "ClusterFirst"});
        }
        Network::None => {}
        Network::Named { name, aliases: own } => {
            labels.insert(net_label(name), "1".into());
            if let Some(a) = own.first() {
                labels.insert(alias_label(name), label_value(a));
            }
            if own.len() > 1 {
                bail!("one --network-alias per container on Kubernetes");
            }
        }
        Network::Container(_) => {
            bail!(
                "--network container:<name> joins that container's Pod (an ephemeral container), not a Pod of its own"
            )
        }
    }

    let mut volumes = vec![json!({
        "name": DATA_VOLUME,
        "persistentVolumeClaim": {"claimName": conf.pvc},
    })];
    // Spare volumes for a container that may join this one's network
    // namespace later (an ephemeral container cannot bring its own).
    for i in 0..GUEST_SLOTS {
        volumes.push(json!({"name": slot_volume(i), "emptyDir": {}}));
    }
    let mut mounts = data_mounts(conf, c)?;
    for (i, t) in c.tmpfs.iter().enumerate() {
        let name = format!("tmpfs-{i}");
        let mut ed = json!({"medium": "Memory"});
        if let Some(s) = tmpfs_size(&t.opts)? {
            ed["sizeLimit"] = json!(s);
        }
        volumes.push(json!({"name": name, "emptyDir": ed}));
        mounts.push(json!({"name": name, "mountPath": t.dst}));
    }
    if let Some(s) = &c.shm_size {
        volumes.push(
            json!({"name": "shm", "emptyDir": {"medium": "Memory", "sizeLimit": quantity(s)?}}),
        );
        mounts.push(json!({"name": "shm", "mountPath": "/dev/shm"}));
    }

    let mut limits = serde_json::Map::new();
    if let Some(m) = &c.limits.memory {
        limits.insert("memory".into(), json!(quantity(m)?));
    }
    if let Some(x) = &c.limits.cpus {
        limits.insert("cpu".into(), json!(cpus(x)?));
    }
    let mut ctr = container(conf, c, "main", mounts)?;
    ctr["resources"] = json!({"limits": limits.clone(), "requests": limits});
    let mut pod_sc = json!({"seccompProfile": {"type": "RuntimeDefault"}});
    if conf.restricted {
        pod_sc["runAsNonRoot"] = json!(true);
    }
    let mut spec = json!({
        "restartPolicy": "Never",
        "automountServiceAccountToken": false,
        "enableServiceLinks": false,
        "shareProcessNamespace": c.init,
        "terminationGracePeriodSeconds": 30,
        "securityContext": pod_sc,
        "containers": [ctr],
        "volumes": volumes,
    });
    if let Some(rc) = &conf.runtime_class {
        spec["runtimeClassName"] = json!(rc);
    }
    for (k, v) in dns.as_object().expect("object") {
        spec[k] = v.clone();
    }
    if c.network != Network::Default {
        spec["initContainers"] = json!([netgate(conf)]);
    }
    // The step's node: with a ReadWriteOnce volume always; with a shared
    // one for containers on a named volume or a directory holding FIFOs,
    // which are node-local IPC (astro-v4's pipes: a FIFO on NFS connects
    // only on one node).
    let pin = conf.same_node
        || c.mounts.iter().any(|m| match &m.src {
            MountSrc::Volume(_) => true,
            MountSrc::Host(p) => holds_fifo(p),
        });
    if pin {
        spec["affinity"] = json!({"nodeAffinity": {"requiredDuringSchedulingIgnoredDuringExecution": {
            "nodeSelectorTerms": [{"matchExpressions": [{
                "key": "kubernetes.io/hostname", "operator": "In", "values": [conf.node]}]}]}}});
    }
    if let Some(p) = &c.platform {
        let arch = p.rsplit('/').next().unwrap_or(p);
        spec["nodeSelector"] = json!({"kubernetes.io/arch": arch});
    }
    place(&mut spec, &conf.node_selector);
    if !aliases.is_empty() {
        let mut by_ip: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (a, ip) in aliases {
            by_ip.entry(ip.as_str()).or_default().push(a.as_str());
        }
        spec["hostAliases"] = json!(
            by_ip
                .into_iter()
                .map(|(ip, names)| json!({"ip": ip, "hostnames": names}))
                .collect::<Vec<_>>()
        );
    }
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": pod_name(c), "namespace": conf.namespace, "labels": labels},
        "spec": spec,
    }))
}

/// `p` is a FIFO or a directory holding one (node-local IPC).
fn holds_fifo(p: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    let fifo = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.file_type().is_fifo());
    fifo(p)
        || std::fs::read_dir(p)
            .map(|d| d.flatten().any(|e| fifo(&e.path())))
            .unwrap_or(false)
}

/// The label a host Pod carries for each guest container it holds (so a
/// guest is found by its own name).
pub fn guest_label(guest: &str) -> String {
    format!("{GUEST_PREFIX}/{}", label_value(guest))
}

/// Spare `emptyDir` volumes every Pod carries for a guest container's
/// mounts (see [`guest_spec`]).
pub const GUEST_SLOTS: usize = 4;

fn slot_volume(i: usize) -> String {
    format!("guest-{i}")
}

/// One mount of a guest: the Pod's spare volume `slot`, filled from
/// `sub` (relative to the evaluation's volume) before the guest starts
/// and, when `back`, copied back there after it ended. `sub` is `None`
/// for a tmpfs (an empty slot).
#[derive(Debug, Clone, PartialEq)]
pub struct GuestCopy {
    pub slot: usize,
    pub sub: Option<String>,
    pub back: bool,
}

/// A container that joins another container's network namespace
/// (`--network container:<name>`): Kubernetes shares a network namespace
/// only within a Pod, so it becomes an ephemeral container of that
/// container's Pod. It reaches the host at 127.0.0.1 and nothing more than
/// the host Pod may (same policies, same runtime class, same node).
///
/// Ephemeral containers may only use their Pod's volumes, without
/// `subPath`, and have no resources of their own. So each of its mounts
/// and tmpfs is one of the Pod's spare `emptyDir`s ([`GUEST_SLOTS`]),
/// filled from the evaluation's volume before it starts and copied back
/// (read-write mounts) after it ended, by a helper ([`copy_helper`]); its
/// `--shm-size` is the Pod's `/dev/shm`; its limits are added to the
/// host's ([`grown`]).
pub fn guest_spec(conf: &Conf, c: &ContainerSpec) -> Result<(Value, Vec<GuestCopy>)> {
    let name = pod_name(c);
    let mut mounts = Vec::new();
    let mut copies = Vec::new();
    for m in &c.mounts {
        let sub = mount_sub(conf, &m.src)?;
        let slot = copies.len();
        mounts
            .push(json!({"name": slot_volume(slot), "mountPath": m.dst, "readOnly": m.read_only}));
        copies.push(GuestCopy {
            slot,
            sub: Some(sub),
            back: !m.read_only,
        });
    }
    for t in &c.tmpfs {
        let slot = copies.len();
        mounts.push(json!({"name": slot_volume(slot), "mountPath": t.dst}));
        copies.push(GuestCopy {
            slot,
            sub: None,
            back: false,
        });
    }
    if copies.len() > GUEST_SLOTS {
        bail!(
            "--network container: on Kubernetes: at most {GUEST_SLOTS} mounts and tmpfs ({} given)",
            copies.len()
        );
    }
    Ok((container(conf, c, &name, mounts)?, copies))
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The helper (ephemeral, crucible's own busybox command) that copies a
/// guest's mounts between the evaluation's volume (`/data`) and the Pod's
/// spare volumes: in (`back` false) every mount, out only the read-write
/// ones. Directories copy their contents; a file mount is refused.
pub fn copy_helper(name: &str, copies: &[GuestCopy], back: bool) -> Option<Value> {
    let mut cmd = vec!["set -e".to_string()];
    let mut mounts = vec![json!({"name": DATA_VOLUME, "mountPath": "/data", "readOnly": !back})];
    for c in copies {
        let Some(sub) = &c.sub else { continue };
        if back && !c.back {
            continue;
        }
        let (data, slot) = (sh_quote(&format!("/data/{sub}")), format!("/g{}", c.slot));
        cmd.push(if back {
            format!("cp -a {slot}/. {data}/")
        } else {
            format!("test -d {data} || {{ echo \"not a directory: {sub}\" >&2; exit 2; }}; cp -a {data}/. {slot}/")
        });
        mounts.push(json!({"name": slot_volume(c.slot), "mountPath": slot}));
    }
    if cmd.len() == 1 {
        return None;
    }
    Some(json!({
        "name": name,
        "image": PROBE_IMAGE,
        "imagePullPolicy": "IfNotPresent",
        "command": ["sh", "-c", cmd.join("\n")],
        "securityContext": {
            "runAsUser": RESTRICTED_UID, "runAsGroup": RESTRICTED_UID, "runAsNonRoot": true,
            "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"]},
            "seccompProfile": {"type": "RuntimeDefault"},
        },
        "volumeMounts": mounts,
    }))
}

/// Kubernetes quantities in base units: bytes (`2Gi`, `512Mi`, `1000`)
/// or millicores (`1500m`, `2`).
fn parse_quantity(q: &str, milli: bool) -> Option<u64> {
    let q = q.trim();
    let (num, mul): (&str, f64) = if let Some(n) = q.strip_suffix('m') {
        (n, 0.001)
    } else {
        let units = [
            ("Ki", 1024.0),
            ("Mi", 1024.0 * 1024.0),
            ("Gi", 1024.0 * 1024.0 * 1024.0),
            ("Ti", 1024.0f64.powi(4)),
            ("k", 1e3),
            ("M", 1e6),
            ("G", 1e9),
            ("T", 1e12),
        ];
        units
            .iter()
            .find_map(|(u, m)| q.strip_suffix(u).map(|n| (n, *m)))
            .unwrap_or((q, 1.0))
    };
    let v: f64 = num.parse().ok()?;
    Some((v * mul * if milli { 1000.0 } else { 1.0 }).round() as u64)
}

/// The host container's resources with a guest's limits added (`None`:
/// nothing to add, or the host has no limit of that kind).
pub fn grown(host: &Value, guest: &super::Limits) -> Result<Option<Value>> {
    let lim = &host["limits"];
    let mut out = serde_json::Map::new();
    if let (Some(h), Some(g)) = (lim["memory"].as_str(), &guest.memory) {
        let (h, g) = (
            parse_quantity(h, false).ok_or_else(|| anyhow!("memory {h}"))?,
            parse_quantity(&quantity(g)?, false).ok_or_else(|| anyhow!("memory {g}"))?,
        );
        out.insert("memory".into(), json!((h + g).to_string()));
    }
    if let (Some(h), Some(g)) = (lim["cpu"].as_str(), &guest.cpus) {
        let (h, g) = (
            parse_quantity(h, true).ok_or_else(|| anyhow!("cpu {h}"))?,
            parse_quantity(&cpus(g)?, true).ok_or_else(|| anyhow!("cpu {g}"))?,
        );
        out.insert("cpu".into(), json!(format!("{}m", h + g)));
    }
    if out.is_empty() {
        return Ok(None);
    }
    let mut res = host.clone();
    for (k, v) in out {
        res["limits"][&k] = v.clone();
        res["requests"][&k] = v;
    }
    Ok(Some(res))
}

/// The Pod name of a container: its `--name`, or for an unnamed one a
/// fresh `ctr-<random>` (Docker would pick a name; a fixed one would make
/// two unnamed containers of one evaluation collide). Unnamed Pods carry
/// the run label like any other, so cleanup by scope still finds them.
pub fn pod_name(c: &ContainerSpec) -> String {
    if !c.name.is_empty() {
        return dns_name(&c.name);
    }
    let mut b = [0u8; 6];
    getrandom::getrandom(&mut b).expect("random");
    format!("ctr-{}", hex::encode(b))
}

/// The init container that holds a Pod back until the network policies
/// apply to it. A new Pod's first second or so is not covered (the policy
/// controller programs its rules after the Pod got its address; measured
/// on k3s: the cluster DNS answers a new Pod's first connection), so
/// nothing of the Pod runs until the cluster DNS, which every policy of a
/// closed network blocks, stops answering. Never blocked within 2 minutes
/// (a CNI that does not enforce NetworkPolicy): the Pod fails.
pub fn netgate(conf: &Conf) -> Value {
    let script = format!(
        "i=0; while nc -w 1 {dns} 53 </dev/null >/dev/null 2>&1; do i=$((i+1)); \
         if [ $i -ge 120 ]; then echo 'network policy not enforced' >&2; exit 1; fi; sleep 1; done",
        dns = conf.dns
    );
    json!({
        "name": "netgate",
        "image": PROBE_IMAGE,
        "imagePullPolicy": "IfNotPresent",
        "command": ["sh", "-c", script],
        "securityContext": {
            "runAsUser": 65534, "runAsGroup": 65534, "runAsNonRoot": true,
            "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true,
            "capabilities": {"drop": ["ALL"]},
            "seccompProfile": {"type": "RuntimeDefault"},
        },
        "resources": {"limits": {"memory": "16Mi", "cpu": "100m"}, "requests": {"memory": "16Mi", "cpu": "100m"}},
    })
}

/// The directory name of a named volume.
fn volume_dir(v: &str) -> Result<String> {
    if v.is_empty()
        || v.len() > 128
        || !v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        || v.starts_with('.')
    {
        bail!("volume name {v:?}");
    }
    Ok(v.to_owned())
}

fn policy(name: &str, ns: &str, labels: &[(String, String)], spec: Value) -> Value {
    let labels: BTreeMap<String, String> = labels
        .iter()
        .map(|(k, v)| (k.clone(), label_value(v)))
        .collect();
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {"name": name, "namespace": ns, "labels": labels},
        "spec": spec,
    })
}

/// The policy of an internal network: members reach members only.
pub fn internal_policy(ns: &str, n: &NetSpec) -> Value {
    let sel = json!({"matchLabels": {net_label(&n.name): "1"}});
    policy(
        &format!("net-{}", dns_name(&n.name)),
        ns,
        &n.labels,
        json!({
            "podSelector": sel,
            "policyTypes": ["Ingress", "Egress"],
            "ingress": [{"from": [{"podSelector": sel}]}],
            "egress": [{"to": [{"podSelector": sel}]}],
        }),
    )
}

/// The two policies of a sandbox: its members reach only `ports` of the
/// step Pod; the step Pod accepts them on those ports.
pub fn sandbox_policies(ns: &str, net: &str, step: &str, ports: &[u16], run: &str) -> [Value; 2] {
    let member = json!({"matchLabels": {net_label(net): "1"}});
    let step_sel = json!({"matchLabels": {STEP_LABEL: label_value(step)}});
    let ports: Vec<Value> = ports
        .iter()
        .map(|p| json!({"protocol": "TCP", "port": p}))
        .collect();
    let labels = [(RUN_LABEL.to_string(), run.to_string())];
    [
        policy(
            &dns_name(net),
            ns,
            &labels,
            json!({
                "podSelector": member,
                "policyTypes": ["Ingress", "Egress"],
                "egress": [{"to": [{"podSelector": step_sel}], "ports": ports}],
            }),
        ),
        policy(
            &format!("{}-step", dns_name(net)),
            ns,
            &labels,
            json!({
                "podSelector": step_sel,
                "policyTypes": ["Ingress"],
                "ingress": [{"from": [{"podSelector": member}], "ports": ports}],
            }),
        ),
    ]
}

/// Pods on the default network: the internet (no private, CGNAT or
/// link-local address) and the cluster DNS.
pub fn internet_policy(ns: &str) -> Value {
    policy(
        "crucible-internet",
        ns,
        &[],
        json!({
            "podSelector": {"matchLabels": {INTERNET: "1"}},
            "policyTypes": ["Egress"],
            "egress": [
                {"to": [{"ipBlock": {"cidr": "0.0.0.0/0", "except": [
                    "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16",
                    "100.64.0.0/10", "169.254.0.0/16", "127.0.0.0/8"]}}]},
                {"to": [{"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "kube-system"}},
                         "podSelector": {"matchLabels": {"k8s-app": "kube-dns"}}}],
                 "ports": [{"protocol": "UDP", "port": 53}, {"protocol": "TCP", "port": 53}]},
            ],
        }),
    )
}

/// The probes a sandbox must block (docs/executors.md §3.4), run in one
/// Pod on the sandbox network: `(name, shell command)`; a command that
/// succeeds means the probe got through.
pub fn probes(conf: &Conf) -> Vec<(&'static str, String)> {
    let api = std::env::var("KUBERNETES_SERVICE_HOST").unwrap_or_else(|_| "10.43.0.1".into());
    let reg = conf.registry.split(':').next().unwrap_or("").to_owned();
    let mut p = vec![
        (
            "dns_cluster",
            format!(
                "nslookup -timeout=5 kubernetes.default.svc.cluster.local {}",
                conf.dns
            ),
        ),
        ("dns_github", "nslookup -timeout=5 github.com".to_string()),
        (
            "http_1_1_1_1",
            "wget -q -T 5 -O /dev/null http://1.1.1.1/".into(),
        ),
        ("tcp_1_1_1_1_443", "nc -w 5 1.1.1.1 443 </dev/null".into()),
        ("tcp_apiserver", format!("nc -w 5 {api} 443 </dev/null")),
        (
            "tcp_metadata",
            "nc -w 5 169.254.169.254 80 </dev/null".into(),
        ),
        (
            "tcp_gateway_kubelet",
            "nc -w 5 $(ip route | awk '/default/ {print $3}') 10250 </dev/null".into(),
        ),
    ];
    if !conf.host_ip.is_empty() {
        p.push((
            "tcp_node_kubelet",
            format!("nc -w 5 {} 10250 </dev/null", conf.host_ip),
        ));
        p.push((
            "tcp_node_ssh",
            format!("nc -w 5 {} 22 </dev/null", conf.host_ip),
        ));
    }
    if !reg.is_empty() {
        p.push((
            "tcp_registry",
            format!("nc -w 5 {} </dev/null", conf.registry.replace(':', " ")),
        ));
    }
    p
}

/// The probe Pod's script: one `name ok|blocked` line per probe.
pub fn probe_script(probes: &[(&str, String)]) -> String {
    let mut s = String::new();
    for (name, cmd) in probes {
        s.push_str(&format!(
            "if {cmd} >/dev/null 2>&1; then echo '{name} ok'; else echo '{name} blocked'; fi\n"
        ));
    }
    s.push_str("echo probes-done\n");
    s
}

/// The probes that got through, from the probe Pod's output; `Err` when
/// the output is incomplete (the probe Pod itself failed).
pub fn probe_failures(out: &str, probes: &[(&str, String)]) -> Result<Vec<String>> {
    if !out.lines().any(|l| l.trim() == "probes-done") {
        bail!(
            "the probe pod did not finish: {}",
            out.chars().take(300).collect::<String>()
        );
    }
    let mut ok = Vec::new();
    for (name, _) in probes {
        let line = out
            .lines()
            .find(|l| l.split_whitespace().next() == Some(name))
            .ok_or_else(|| anyhow!("probe {name}: no result"))?;
        if line.trim_end().ends_with(" ok") {
            ok.push((*name).to_owned());
        }
    }
    Ok(ok)
}

pub struct K8sExecutor {
    pub conf: Conf,
}

/// `kubectl` with the given stdin; stdout, or the error with kubectl's
/// own message.
async fn kubectl(args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("kubectl");
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running kubectl")?;
    if let (Some(data), Some(mut w)) = (stdin, child.stdin.take()) {
        w.write_all(data).await?;
        drop(w);
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "kubectl {}: {}",
            args.first().unwrap_or(&""),
            err.trim().chars().take(400).collect::<String>()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The state of a Pod's main container.
#[derive(Debug, Default)]
struct PodState {
    exists: bool,
    phase: String,
    running: bool,
    exit_code: Option<i64>,
    /// Why it is still waiting (image pull errors ...).
    waiting: Option<String>,
    ip: Option<String>,
}

fn pod_state(v: &Value) -> PodState {
    let cs = v["status"]["containerStatuses"]
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    state_of(v, &cs)
}

/// The state of ephemeral container `name` of Pod `v`.
fn guest_state(v: &Value, name: &str) -> PodState {
    let cs = v["status"]["ephemeralContainerStatuses"]
        .as_array()
        .and_then(|a| a.iter().find(|c| c["name"] == name))
        .cloned()
        .unwrap_or(Value::Null);
    state_of(v, &cs)
}

fn state_of(v: &Value, cs: &Value) -> PodState {
    let st = &v["status"];
    let state = &cs["state"];
    PodState {
        exists: true,
        phase: st["phase"].as_str().unwrap_or("").to_owned(),
        running: state["running"].is_object(),
        exit_code: state["terminated"]["exitCode"].as_i64(),
        waiting: state["waiting"]["reason"].as_str().map(str::to_owned),
        ip: st["podIP"].as_str().map(str::to_owned),
    }
}

/// Where a container is: its own Pod (`main`), or an ephemeral container
/// of its host's Pod.
#[derive(Debug, Clone, PartialEq)]
struct Loc {
    pod: String,
    container: String,
    guest: bool,
}

impl K8sExecutor {
    pub fn from_env() -> Result<K8sExecutor> {
        Ok(K8sExecutor {
            conf: Conf::from_env()?,
        })
    }

    fn ns(&self) -> &str {
        &self.conf.namespace
    }

    async fn get_pod_json(&self, name: &str) -> Result<Option<Value>> {
        match kubectl(&["-n", self.ns(), "get", "pod", name, "-o", "json"], None).await {
            Ok(out) => Ok(Some(serde_json::from_str(&out)?)),
            Err(e) if e.to_string().contains("NotFound") => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn get_pod(&self, name: &str) -> Result<PodState> {
        Ok(self
            .get_pod_json(&dns_name(name))
            .await?
            .map(|v| pod_state(&v))
            .unwrap_or_default())
    }

    /// Container `id`: its own Pod, else the Pod holding it as a guest
    /// (`None`: neither exists).
    async fn locate(&self, id: &str) -> Result<Option<(Loc, PodState)>> {
        let name = dns_name(id);
        if let Some(v) = self.get_pod_json(&name).await? {
            let loc = Loc {
                pod: name,
                container: "main".into(),
                guest: false,
            };
            return Ok(Some((loc, pod_state(&v))));
        }
        let out = kubectl(
            &[
                "-n",
                self.ns(),
                "get",
                "pods",
                "-l",
                &format!("{}=1", guest_label(&name)),
                "-o",
                "json",
            ],
            None,
        )
        .await?;
        let v: Value = serde_json::from_str(&out)?;
        Ok(v["items"].as_array().and_then(|a| a.first()).map(|p| {
            let loc = Loc {
                pod: p["metadata"]["name"].as_str().unwrap_or("").to_owned(),
                container: name.clone(),
                guest: true,
            };
            (loc, guest_state(p, &name))
        }))
    }

    /// Add ephemeral container `ctr` to Pod `host`.
    async fn add_ephemeral(&self, host: &str, ctr: &Value) -> Result<()> {
        let patch = json!({"spec": {"ephemeralContainers": [ctr]}});
        kubectl(
            &[
                "-n",
                self.ns(),
                "patch",
                "pod",
                host,
                "--subresource",
                "ephemeralcontainers",
                "--type",
                "strategic",
                "-p",
                &patch.to_string(),
            ],
            None,
        )
        .await
        .map(|_| ())
    }

    /// Wait until ephemeral container `name` of `host` runs (`until_end`:
    /// has ended); its state.
    async fn wait_ephemeral(&self, host: &str, name: &str, until_end: bool) -> Result<PodState> {
        let t0 = Instant::now();
        loop {
            let s = self
                .get_pod_json(host)
                .await?
                .map(|v| guest_state(&v, name))
                .unwrap_or_default();
            if !s.exists {
                bail!("{host} went away before {name} ended");
            }
            if s.exit_code.is_some() || (s.running && !until_end) {
                return Ok(s);
            }
            if let Some(w) = &s.waiting
                && matches!(
                    w.as_str(),
                    "ErrImagePull"
                        | "ImagePullBackOff"
                        | "InvalidImageName"
                        | "CreateContainerError"
                )
                && t0.elapsed() > Duration::from_secs(600)
            {
                bail!("{name} in {host}: {w}");
            }
            if !until_end && t0.elapsed() > Duration::from_secs(1800) {
                bail!("{name} in {host} did not start within 30 minutes");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Run a copy helper ([`copy_helper`]) for guest `name` in `host`.
    async fn copy(&self, host: &str, name: &str, copies: &[GuestCopy], back: bool) -> Result<()> {
        let helper = format!("{}-{}", name, if back { "out" } else { "in" });
        let helper = dns_name(&helper);
        let Some(h) = copy_helper(&helper, copies, back) else {
            return Ok(());
        };
        self.add_ephemeral(host, &h).await?;
        let s = self.wait_ephemeral(host, &helper, true).await?;
        if s.exit_code != Some(0) {
            let why = kubectl(&["-n", self.ns(), "logs", host, "-c", &helper], None)
                .await
                .unwrap_or_default();
            bail!(
                "copying {name}'s mounts {}: {}",
                if back { "back" } else { "in" },
                why.trim()
            );
        }
        Ok(())
    }

    /// Start `c` as an ephemeral container of `target`'s Pod (see
    /// [`guest_spec`]): its name, host Pod and mounts.
    async fn join(
        &self,
        c: &ContainerSpec,
        target: &str,
    ) -> Result<(String, String, Vec<GuestCopy>)> {
        let host = dns_name(target);
        let Some(pod) = self.get_pod_json(&host).await? else {
            bail!("--network container:{target}: no such container");
        };
        if !pod_state(&pod).running {
            bail!("--network container:{target}: it is not running");
        }
        if pod["spec"]["shareProcessNamespace"] == true {
            // The host's processes would see the copy helper's mounts.
            bail!(
                "--network container:{target}: not with a host started with --init on Kubernetes"
            );
        }
        let (ctr, copies) = guest_spec(&self.conf, c)?;
        let name = ctr["name"].as_str().unwrap_or("").to_owned();
        // Ephemeral containers have no resources: the host's grow by the
        // guest's (in-place resize), so the Pod as a whole has both.
        let main = pod["spec"]["containers"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == "main"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(res) = grown(&main["resources"], &c.limits)? {
            let patch = json!({"spec": {"containers": [{"name": "main", "resources": res}]}});
            if let Err(e) = kubectl(
                &[
                    "-n",
                    self.ns(),
                    "patch",
                    "pod",
                    &host,
                    "--subresource",
                    "resize",
                    "--type",
                    "strategic",
                    "-p",
                    &patch.to_string(),
                ],
                None,
            )
            .await
            {
                eprintln!("warning: {name} shares {host}'s resources (resize failed: {e})");
            }
        }
        self.copy(&host, &name, &copies, false).await?;
        kubectl(
            &[
                "-n",
                self.ns(),
                "label",
                "pod",
                &host,
                &format!("{}=1", guest_label(&name)),
            ],
            None,
        )
        .await?;
        self.add_ephemeral(&host, &ctr).await?;
        self.wait_ephemeral(&host, &name, false).await?;
        Ok((name, host, copies))
    }

    /// (alias, IP) of the running members of `net` that have an alias.
    async fn aliases(&self, net: &str) -> Result<Vec<(String, String)>> {
        let out = kubectl(
            &[
                "-n",
                self.ns(),
                "get",
                "pods",
                "-l",
                &format!("{}=1", net_label(net)),
                "-o",
                "json",
            ],
            None,
        )
        .await?;
        let v: Value = serde_json::from_str(&out)?;
        let key = alias_label(net);
        Ok(v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let a = p["metadata"]["labels"][&key].as_str()?;
                let ip = p["status"]["podIP"].as_str()?;
                Some((a.to_owned(), ip.to_owned()))
            })
            .collect())
    }

    async fn apply(&self, obj: &Value) -> Result<()> {
        kubectl(&["apply", "-f", "-"], Some(obj.to_string().as_bytes()))
            .await
            .map(|_| ())
    }

    /// Create the Pod of `c`; wait until its container started (or ended).
    async fn create(&self, c: &ContainerSpec) -> Result<String> {
        if let Network::Container(target) = &c.network {
            // Detached: what it writes stays in the host Pod (not copied back).
            return Ok(self.join(c, target).await?.0);
        }
        if c.network == Network::Default {
            self.apply(&internet_policy(self.ns())).await?;
        }
        let aliases = match &c.network {
            Network::Named { name, .. } => self.aliases(name).await?,
            _ => Vec::new(),
        };
        let pod = pod_spec(&self.conf, c, &aliases)?;
        let name = pod["metadata"]["name"].as_str().unwrap_or("").to_owned();
        kubectl(&["create", "-f", "-"], Some(pod.to_string().as_bytes())).await?;
        // Image pulls and scheduling: up to 30 minutes (large images).
        let t0 = Instant::now();
        loop {
            let s = self.get_pod(&name).await?;
            if s.phase == "Failed" && s.exit_code.is_none() && !s.running {
                let why = kubectl(
                    &[
                        "-n",
                        self.ns(),
                        "logs",
                        &name,
                        "-c",
                        "netgate",
                        "--tail",
                        "3",
                    ],
                    None,
                )
                .await
                .unwrap_or_default();
                self.remove(&name).await;
                bail!(
                    "pod {name} failed before its container started: {}",
                    why.trim()
                );
            }
            if s.running || s.exit_code.is_some() || s.phase == "Failed" || s.phase == "Succeeded" {
                // Aliases need the address.
                if s.running && s.ip.is_none() {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                return Ok(name);
            }
            if let Some(w) = &s.waiting
                && matches!(
                    w.as_str(),
                    "ErrImagePull"
                        | "ImagePullBackOff"
                        | "InvalidImageName"
                        | "CreateContainerConfigError"
                        | "CreateContainerError"
                )
                && t0.elapsed()
                    > Duration::from_secs(if w == "ImagePullBackOff" { 600 } else { 120 })
            {
                self.remove(&name).await;
                bail!("pod {name}: {w}");
            }
            if t0.elapsed() > Duration::from_secs(1800) {
                self.remove(&name).await;
                bail!("pod {name} did not start within 30 minutes");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Run a probe Pod on sandbox network `net`; the probes that got through.
    async fn probe(&self, net: &str, run: &str) -> Result<Vec<String>> {
        let probes = probes(&self.conf);
        let c = ContainerSpec {
            name: format!("{}-probe", dns_name(net)),
            image: PROBE_IMAGE.into(),
            entrypoint: Some(vec!["sh".into(), "-c".into(), probe_script(&probes)]),
            user: Some("65534:65534".into()),
            network: Network::Named {
                name: net.into(),
                aliases: vec![],
            },
            dns: Some("127.0.0.1".into()),
            labels: vec![(RUN_LABEL.into(), run.into())],
            cap_drop: vec!["ALL".into()],
            no_new_privileges: true,
            limits: super::Limits {
                memory: Some("64m".into()),
                cpus: Some("0.5".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let name = self.create(&c).await?;
        let _ = self.wait(&name).await;
        let out = kubectl(&["-n", self.ns(), "logs", "-c", "main", &name], None)
            .await
            .unwrap_or_default();
        self.remove(&name).await;
        for l in out.lines() {
            eprintln!("sandbox check {l}");
        }
        probe_failures(&out, &probes)
    }
}

impl Executor for K8sExecutor {
    fn caps(&self) -> Caps {
        Caps {
            sandbox_net: true,
            pids_limit: self.conf.pids_limit,
        }
    }

    fn image_ref(&self, name: &str) -> String {
        map_image(&self.conf.registry, name)
    }

    fn sandbox(&self, ports: &[u16], label: &str) -> Result<Sandbox> {
        let net = format!("crucible-sbx-{}", dns_name(label));
        let net = dns_name(&net);
        let pols = sandbox_policies(self.ns(), &net, &self.conf.step, ports, label);
        let rt = tokio::runtime::Handle::current();
        let this = K8sExecutor {
            conf: self.conf.clone(),
        };
        let r = tokio::task::block_in_place(|| {
            rt.block_on(async {
                for p in &pols {
                    this.apply(p).await?;
                }
                this.probe(&net, label).await
            })
        });
        let (ns, n2) = (self.ns().to_owned(), net.clone());
        let sb = Sandbox::new(
            net.clone(),
            self.conf.pod_ip.clone(),
            Box::new(move || {
                let _ = std::process::Command::new("kubectl")
                    .args(["-n", &ns, "delete", "networkpolicy", "--ignore-not-found"])
                    .arg(dns_name(&n2))
                    .arg(format!("{}-step", dns_name(&n2)))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }),
        );
        match r {
            Ok(through) if through.is_empty() => {
                eprintln!("sandbox {net}: host {} ports {ports:?}", self.conf.pod_ip);
                Ok(sb)
            }
            Ok(through) => bail!(
                "sandbox check failed: {} got through (does the CNI enforce NetworkPolicy?): refusing to run",
                through.join(", ")
            ),
            Err(e) => Err(e.context("sandbox check")),
        }
    }

    async fn start(&self, c: &ContainerSpec) -> Result<String> {
        self.create(c).await
    }

    async fn run_attached(&self, c: &ContainerSpec, rm: bool) -> Result<i32> {
        let (name, guest) = match &c.network {
            Network::Container(target) => {
                let (name, host, copies) = self.join(c, target).await?;
                (name, Some((host, copies)))
            }
            _ => (self.create(c).await?, None),
        };
        // Its output, while it runs and after it ended.
        if let Some((loc, _)) = self.locate(&name).await? {
            let _ = tokio::process::Command::new("kubectl")
                .args([
                    "-n",
                    self.ns(),
                    "logs",
                    "-f",
                    "-c",
                    &loc.container,
                    &loc.pod,
                ])
                .stdin(Stdio::null())
                .status()
                .await;
        }
        let code = self.wait(&name).await?;
        if let Some((host, copies)) = guest {
            // What it wrote to its read-write mounts.
            self.copy(&host, &name, &copies, true).await?;
        }
        if rm {
            self.remove(&name).await;
        }
        Ok(code.map_or(125, |c| c as i32))
    }

    async fn wait(&self, id: &str) -> Result<Option<i64>> {
        loop {
            let Some((_, s)) = self.locate(id).await? else {
                return Ok(None);
            };
            if s.exit_code.is_some() {
                return Ok(s.exit_code);
            }
            if s.phase == "Failed" && !s.running {
                // Killed before its container ran (deadline, eviction).
                return Ok(Some(137));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn stop(&self, id: &str, _grace: Duration) {
        // A guest cannot be stopped on its own (Kubernetes has no call for
        // it): it ends with its process or with its host.
        if let Ok(Some((loc, _))) = self.locate(id).await
            && loc.guest
        {
            return;
        }
        // The pod stays (logs, exit code): its deadline ends it now, with
        // its termination grace period.
        let _ = kubectl(
            &[
                "-n",
                self.ns(),
                "patch",
                "pod",
                &dns_name(id),
                "--type",
                "merge",
                "-p",
                r#"{"spec":{"activeDeadlineSeconds":1}}"#,
            ],
            None,
        )
        .await;
    }

    async fn inspect(&self, id: &str) -> Result<State> {
        let Some((_, s)) = self.locate(id).await? else {
            bail!("no such container: {id}");
        };
        Ok(State {
            running: s.running,
            exit_code: s.exit_code,
        })
    }

    async fn logs(&self, id: &str, tail: usize, to: &Path) -> Result<()> {
        let (loc, _) = self
            .locate(id)
            .await?
            .ok_or_else(|| anyhow!("no such container: {id}"))?;
        let out = kubectl(
            &[
                "-n",
                self.ns(),
                "logs",
                "--tail",
                &tail.to_string(),
                "-c",
                &loc.container,
                &loc.pod,
            ],
            None,
        )
        .await?;
        std::fs::write(to, out)?;
        Ok(())
    }

    async fn print_logs(&self, id: &str, tail: Option<usize>) -> Result<i32> {
        let Some((loc, _)) = self.locate(id).await? else {
            eprintln!("no such container: {id}");
            return Ok(1);
        };
        let mut a = vec![
            "-n".to_string(),
            self.ns().to_owned(),
            "logs".into(),
            "-c".into(),
            loc.container,
        ];
        if let Some(t) = tail {
            a.extend(["--tail".into(), t.to_string()]);
        }
        a.push(loc.pod);
        let st = tokio::process::Command::new("kubectl")
            .args(&a)
            .stdin(Stdio::null())
            .status()
            .await?;
        Ok(st.code().unwrap_or(1))
    }

    async fn remove(&self, id: &str) {
        // A guest stays in its host's Pod (ephemeral containers cannot be
        // removed) and goes with it.
        if let Ok(Some((loc, _))) = self.locate(id).await
            && loc.guest
        {
            return;
        }
        let _ = kubectl(
            &[
                "-n",
                self.ns(),
                "delete",
                "pod",
                &dns_name(id),
                "--ignore-not-found",
                "--grace-period=1",
                "--wait=true",
            ],
            None,
        )
        .await;
    }

    async fn build(&self, b: &BuildSpec, log: &Path) -> Result<()> {
        if b.network.is_some() {
            bail!("builds on a sandbox network are not supported on Kubernetes");
        }
        let f = std::fs::File::create(log)?;
        let st = tokio::process::Command::new("buildctl")
            .args(buildctl_args(&self.conf, b))
            .stdin(Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f)
            .kill_on_drop(true)
            .status()
            .await
            .context("running buildctl")?;
        if !st.success() {
            bail!("build of {} failed ({st})", b.tag);
        }
        Ok(())
    }

    async fn build_attached(&self, b: &BuildSpec) -> Result<i32> {
        // buildctl is our child: forward termination (a `timeout` around
        // `crucible ctr build`) so the build is cancelled.
        let mut child = tokio::process::Command::new("buildctl")
            .args(buildctl_args(&self.conf, b))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("running buildctl")?;
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        tokio::select! {
            st = child.wait() => Ok(st?.code().unwrap_or(1)),
            _ = term.recv() => { let _ = child.kill().await; Ok(143) }
            _ = int.recv() => { let _ = child.kill().await; Ok(130) }
        }
    }

    async fn image_exists(&self, image: &str) -> bool {
        let r = self.image_ref(image);
        match registry_path(&self.conf.registry, &r) {
            Some((repo, tag)) => manifest_digest(&self.conf.registry, &repo, &tag)
                .await
                .is_ok_and(|d| d.is_some()),
            // Not ours: the node pulls it when a Pod needs it.
            None => true,
        }
    }

    async fn image_pull(&self, _image: &str) -> Result<()> {
        Ok(())
    }

    async fn image_rm(&self, image: &str) {
        let r = self.image_ref(image);
        if let Some((repo, tag)) = registry_path(&self.conf.registry, &r)
            && let Ok(Some(d)) = manifest_digest(&self.conf.registry, &repo, &tag).await
        {
            let _ = reqwest::Client::new()
                .delete(format!(
                    "http://{}/v2/{repo}/manifests/{d}",
                    self.conf.registry
                ))
                .timeout(Duration::from_secs(30))
                .send()
                .await;
        }
    }

    async fn image_save(&self, _image: &str, _to: &Path) -> Result<()> {
        bail!("saving images is not supported on Kubernetes (plugins are registered on Docker)")
    }

    async fn image_load(&self, from: &Path, tag: &str) -> Result<()> {
        let r = self.image_ref(tag);
        let (repo, tag) = registry_path(&self.conf.registry, &r)
            .ok_or_else(|| anyhow!("{r} is not in the cluster registry"))?;
        let a = crate::image_archive::Archive::parse(std::fs::read(from)?)?;
        push_archive(&self.conf.registry, &repo, &tag, &a).await
    }

    async fn prune_build_cache(&self) {}

    async fn net_create(&self, n: &NetSpec) -> Result<()> {
        if !n.internal {
            bail!("only internal networks");
        }
        let mut n = n.clone();
        if let Ok(l) = std::env::var("CRUCIBLE_RUN_LABEL")
            && !l.is_empty()
        {
            n.labels.push((RUN_LABEL.into(), l));
        }
        self.apply(&internal_policy(self.ns(), &n)).await
    }

    async fn net_connect(&self, net: &str, id: &str, aliases: &[String]) -> Result<()> {
        let mut a = vec![
            "-n".to_string(),
            self.ns().to_owned(),
            "label".into(),
            "--overwrite".into(),
            "pod".into(),
            dns_name(id),
            format!("{}=1", net_label(net)),
        ];
        match aliases {
            [] => {}
            [al] => a.push(format!("{}={}", alias_label(net), label_value(al))),
            _ => bail!("one alias per container on Kubernetes"),
        }
        kubectl(&a.iter().map(String::as_str).collect::<Vec<_>>(), None)
            .await
            .map(|_| ())
    }

    async fn net_rm(&self, net: &str) {
        let _ = kubectl(
            &[
                "-n",
                self.ns(),
                "delete",
                "networkpolicy",
                "--ignore-not-found",
                &format!("net-{}", dns_name(net)),
            ],
            None,
        )
        .await;
    }

    async fn volume_create(&self, name: &str, _labels: &[(String, String)]) -> Result<()> {
        let d = self.conf.root.join(".volumes").join(volume_dir(name)?);
        std::fs::create_dir_all(&d)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777))?;
        Ok(())
    }

    async fn volume_rm(&self, name: &str) {
        if let Ok(v) = volume_dir(name) {
            let _ = std::fs::remove_dir_all(self.conf.root.join(".volumes").join(v));
        }
    }

    fn cleanup(&self, label: &str) {
        let _ = std::process::Command::new("kubectl")
            .args(["-n", self.ns(), "delete", "pods,networkpolicies", "-l"])
            .arg(format!("{RUN_LABEL}={}", label_value(label)))
            .args(["--ignore-not-found", "--grace-period=1", "--wait=true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// `buildctl` arguments of a build: Dockerfile frontend, pushed to the
/// registry under the mapped tag.
pub fn buildctl_args(conf: &Conf, b: &BuildSpec) -> Vec<String> {
    let dir = b.dir.display().to_string();
    let mut a = vec![
        "--addr".to_string(),
        conf.buildkit.clone(),
        "build".into(),
        "--frontend".into(),
        "dockerfile.v0".into(),
        "--local".into(),
        format!("context={dir}"),
        "--local".into(),
        format!("dockerfile={dir}"),
        "--progress".into(),
        "plain".into(),
    ];
    if b.no_network {
        a.extend(["--opt".into(), "force-network-mode=none".into()]);
    }
    if let Some(p) = &b.platform {
        a.extend(["--opt".into(), format!("platform={p}")]);
    }
    for x in &b.build_args {
        a.extend(["--opt".into(), format!("build-arg:{x}")]);
    }
    for (k, v) in &b.labels {
        a.extend(["--opt".into(), format!("label:{k}={v}")]);
    }
    a.extend([
        "--output".into(),
        format!(
            "type=image,name={},push=true,registry.insecure=true",
            map_image(&conf.registry, &b.tag)
        ),
    ]);
    a
}

/// `(repository, tag)` of a reference in the registry, if it is there.
pub fn registry_path(registry: &str, reference: &str) -> Option<(String, String)> {
    let rest = reference.strip_prefix(&format!("{registry}/"))?;
    let (repo, tag) = rest.rsplit_once(':').unwrap_or((rest, "latest"));
    Some((repo.to_owned(), tag.to_owned()))
}

const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";

/// The digest of `repo:tag` in the registry (`None`: not there).
async fn manifest_digest(registry: &str, repo: &str, tag: &str) -> Result<Option<String>> {
    let r = reqwest::Client::new()
        .head(format!("http://{registry}/v2/{repo}/manifests/{tag}"))
        .header("Accept", MANIFEST_TYPES)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !r.status().is_success() {
        bail!("registry: {}", r.status());
    }
    Ok(r.headers()
        .get("Docker-Content-Digest")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned))
}

/// Push a `docker save` archive to the cluster registry as an OCI image
/// (config digest = the image id the archive was checked against).
pub async fn push_archive(
    registry: &str,
    repo: &str,
    tag: &str,
    a: &crate::image_archive::Archive,
) -> Result<()> {
    let client = reqwest::Client::new();
    let base = format!("http://{registry}/v2/{repo}");
    let file = |n: &str| a.file(n).ok_or_else(|| anyhow!("{n} missing"));
    let put = |bytes: Vec<u8>| {
        let client = client.clone();
        let base = base.clone();
        async move {
            let digest = format!("sha256:{}", crucible_store::sha256_hex(&bytes));
            let start = client
                .post(format!("{base}/blobs/uploads/"))
                .timeout(Duration::from_secs(60))
                .send()
                .await?
                .error_for_status()?;
            let loc = start
                .headers()
                .get("Location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("registry: no upload location"))?
                .to_owned();
            let url = if loc.starts_with("http") {
                loc
            } else {
                format!("http://{registry}{loc}")
            };
            let sep = if url.contains('?') { '&' } else { '?' };
            let size = bytes.len();
            client
                .put(format!("{url}{sep}digest={digest}"))
                .header("Content-Type", "application/octet-stream")
                .body(bytes)
                .timeout(Duration::from_secs(600))
                .send()
                .await?
                .error_for_status()?;
            Ok::<_, anyhow::Error>((digest, size))
        }
    };
    let cfg = file(&a.config)?.to_vec();
    let (cd, cs) = put(cfg).await?;
    let mut layers = Vec::new();
    for l in &a.layers {
        let b = file(l)?;
        let mt = crate::image_archive::layer_media_type(b);
        let (d, s) = put(b.to_vec()).await?;
        layers.push(json!({"mediaType": mt, "digest": d, "size": s}));
    }
    let m = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": cd, "size": cs},
        "layers": layers,
    });
    client
        .put(format!("{base}/manifests/{tag}"))
        .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
        .body(serde_json::to_vec(&m)?)
        .timeout(Duration::from_secs(60))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Size (compressed layers) and `/agent-build.json` (at most 2000 bytes,
/// `{}` when absent) of an image in the registry, read from its layers,
/// newest first; the image is never run.
pub async fn image_facts(conf: &Conf, reference: &str) -> Result<(u64, Value)> {
    let (repo, tag) = registry_path(&conf.registry, reference)
        .ok_or_else(|| anyhow!("{reference} is not in the cluster registry"))?;
    let client = reqwest::Client::new();
    let base = format!("http://{}/v2/{repo}", conf.registry);
    let get = |what: String| {
        client
            .get(format!("{base}/{what}"))
            .header("Accept", MANIFEST_TYPES)
            .timeout(Duration::from_secs(60))
            .send()
    };
    let mut m: Value = get(format!("manifests/{tag}"))
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(list) = m["manifests"].as_array() {
        // An index: the linux image (not an attestation).
        let d = list
            .iter()
            .find(|x| x["platform"]["os"] == "linux")
            .and_then(|x| x["digest"].as_str())
            .ok_or_else(|| anyhow!("{reference}: no linux image in the index"))?
            .to_owned();
        m = get(format!("manifests/{d}"))
            .await?
            .error_for_status()?
            .json()
            .await?;
    }
    let layers = m["layers"].as_array().cloned().unwrap_or_default();
    let size: u64 = layers.iter().filter_map(|l| l["size"].as_u64()).sum();
    for l in layers.iter().rev() {
        let Some(d) = l["digest"].as_str() else {
            continue;
        };
        let blob = get(format!("blobs/{d}"))
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let tmp = tempfile::tempdir()?;
        let f = tmp.path().join("layer");
        std::fs::write(&f, &blob)?;
        drop(blob);
        let out = tokio::process::Command::new("tar")
            .arg("-xzOf")
            .arg(&f)
            .arg("agent-build.json")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await?;
        if out.status.success() {
            let v = (out.stdout.len() <= 2000)
                .then(|| serde_json::from_slice::<Value>(&out.stdout).ok())
                .flatten()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            return Ok((size, v));
        }
    }
    Ok((size, json!({})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{Limits, Mount, Tmpfs};

    fn conf() -> Conf {
        Conf {
            namespace: "crucible-e-x".into(),
            root: "/crucible".into(),
            pvc: "data".into(),
            step: "crucible-x-score-tests-r1".into(),
            pod_ip: "10.42.0.9".into(),
            node: "n1".into(),
            host_ip: "172.28.1.2".into(),
            registry: "10.43.200.200:5000".into(),
            buildkit: "tcp://buildkitd.crucible-system:1234".into(),
            dns: "10.43.0.10".into(),
            pids_limit: true,
            same_node: true,
            node_selector: vec![],
            restricted: false,
            runtime_class: None,
        }
    }

    fn agent() -> ContainerSpec {
        crate::runners::workdir::agent_container(&crate::runners::workdir::ContainerSpec {
            name: "crucible-stage-1-7".into(),
            image: "crucible-agent-e1:r1".into(),
            network: "crucible-sbx-e1-r1".into(),
            label: "e1-r1".into(),
            user: "1000:1000".into(),
            req: "/crucible/work-r1/inputs/stage-1".into(),
            work: "/crucible/work-r1/scratch/work".into(),
            home: "/crucible/work-r1/scratch/home".into(),
            agent_host: "10.42.0.9".into(),
            meter_port: 8787,
            egress_port: 3128,
            model: "m".into(),
            deadline_s: 60,
            entrypoint: Some(vec!["/run.sh".into()]),
        })
    }

    #[test]
    fn agent_pod_is_locked_down() {
        let p = pod_spec(&conf(), &agent(), &[]).unwrap();
        let s = &p["spec"];
        let c = &s["containers"][0];
        assert_eq!(p["metadata"]["namespace"], "crucible-e-x");
        assert_eq!(p["metadata"]["labels"]["crucible.run"], "e1-r1");
        assert_eq!(
            p["metadata"]["labels"]["net.crucible/crucible-sbx-e1-r1"],
            "1"
        );
        assert_eq!(c["image"], "10.43.200.200:5000/crucible-agent-e1:r1");
        assert_eq!(c["securityContext"]["allowPrivilegeEscalation"], false);
        assert_eq!(c["securityContext"]["capabilities"]["drop"], json!(["ALL"]));
        assert_eq!(c["securityContext"]["capabilities"]["add"], json!([]));
        assert_eq!(c["securityContext"]["runAsUser"], 1000);
        assert_eq!(c["securityContext"]["runAsNonRoot"], true);
        assert_eq!(
            c["securityContext"]["seccompProfile"]["type"],
            "RuntimeDefault"
        );
        assert_eq!(
            c["resources"]["limits"],
            json!({"memory": "2Gi", "cpu": "1000m"})
        );
        assert_eq!(c["resources"]["requests"], c["resources"]["limits"]);
        assert_eq!(s["automountServiceAccountToken"], false);
        assert_eq!(s["enableServiceLinks"], false);
        assert_eq!(s["shareProcessNamespace"], true);
        assert_eq!(s["restartPolicy"], "Never");
        assert_eq!(s["dnsPolicy"], "None");
        assert_eq!(s["dnsConfig"]["nameservers"], json!(["127.0.0.1"]));
        assert!(s.get("hostNetwork").is_none() && s.get("serviceAccountName").is_none());
        // Nothing runs before the policies apply to the pod.
        assert_eq!(s["initContainers"][0]["name"], "netgate");
        assert!(
            s["initContainers"][0]["command"][2]
                .as_str()
                .unwrap()
                .contains("nc -w 1 10.43.0.10 53")
        );
        // Same node as the step (its local volume).
        assert_eq!(
            s["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
                [0]["matchExpressions"][0]["values"],
            json!(["n1"])
        );
        // Mounts: subPaths of the evaluation's volume.
        let m = c["volumeMounts"].as_array().unwrap();
        assert_eq!(
            m[0],
            json!({"name": "data", "mountPath": "/req", "readOnly": true, "subPath": "work-r1/inputs/stage-1"})
        );
        assert_eq!(m[1]["subPath"], "work-r1/scratch/work");
        assert_eq!(m[1]["readOnly"], false);
        assert_eq!(
            s["volumes"][0]["persistentVolumeClaim"]["claimName"],
            "data"
        );
        let env = c["env"].as_array().unwrap();
        assert!(
            env.contains(&json!({"name": "OPENAI_BASE_URL", "value": "http://10.42.0.9:8787/v1"}))
        );
        assert_eq!(c["command"], json!(["/run.sh"]));
    }

    #[test]
    fn shared_volume_pods_go_to_the_sandbox_pool_not_the_step_node() {
        // Single node (default): no pool, the step's node, as before.
        let s = pod_spec(&conf(), &agent(), &[]).unwrap()["spec"].clone();
        assert!(s.get("nodeSelector").is_none() && s.get("tolerations").is_none());
        // ReadWriteMany volume and a sandbox pool.
        let mut cf = conf();
        cf.same_node = false;
        cf.node_selector = parse_selector("crucible/pool=sandbox").unwrap();
        let s = pod_spec(&cf, &agent(), &[]).unwrap()["spec"].clone();
        assert!(s.get("affinity").is_none());
        assert_eq!(s["nodeSelector"], json!({"crucible/pool": "sandbox"}));
        // A named volume (FIFOs): the step's node, still in the pool.
        let mut c = agent();
        c.mounts.push(Mount {
            src: MountSrc::Volume("pipes".into()),
            dst: "/pipes".into(),
            read_only: false,
        });
        let s = pod_spec(&cf, &c, &[]).unwrap()["spec"].clone();
        assert_eq!(
            s["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]["nodeSelectorTerms"]
                [0]["matchExpressions"][0]["values"],
            json!(["n1"])
        );
        assert_eq!(s["nodeSelector"], json!({"crucible/pool": "sandbox"}));
        assert_eq!(
            s["tolerations"],
            json!([{"key": "crucible/pool", "operator": "Equal", "value": "sandbox", "effect": "NoSchedule"}])
        );
        // A directory holding FIFOs (astro-v4's pipes): the step's node too.
        let root = tempfile::tempdir().unwrap();
        let (pipes, plain) = (root.path().join("pipes"), root.path().join("plain"));
        std::fs::create_dir(&pipes).unwrap();
        std::fs::create_dir(&plain).unwrap();
        let st = std::process::Command::new("mkfifo")
            .arg(pipes.join("to_agent"))
            .status()
            .unwrap();
        assert!(st.success());
        let mut fc = cf.clone();
        fc.root = root.path().to_path_buf();
        let mut c = agent();
        c.mounts = vec![Mount::host(&pipes, "/pipes", false)];
        let s = pod_spec(&fc, &c, &[]).unwrap()["spec"].clone();
        assert!(s.get("affinity").is_some());
        c.mounts = vec![Mount::host(&plain, "/x", false)];
        assert!(
            pod_spec(&fc, &c, &[]).unwrap()["spec"]
                .get("affinity")
                .is_none()
        );
        // --platform keeps the pool.
        let mut c = agent();
        c.platform = Some("linux/arm64".into());
        let s = pod_spec(&cf, &c, &[]).unwrap()["spec"].clone();
        assert_eq!(
            s["nodeSelector"],
            json!({"kubernetes.io/arch": "arm64", "crucible/pool": "sandbox"})
        );
    }

    #[test]
    fn node_selectors_parse() {
        assert_eq!(parse_selector("").unwrap(), vec![]);
        assert_eq!(
            parse_selector("crucible/pool=sandbox, zone=a").unwrap(),
            vec![
                ("crucible/pool".to_string(), "sandbox".to_string()),
                ("zone".into(), "a".into())
            ]
        );
        assert_eq!(
            selector_string(&parse_selector("a=b,c=d").unwrap()),
            "a=b,c=d"
        );
        for bad in ["pool", "=x", "a=b c", "a=b/c", "a b=c"] {
            assert!(parse_selector(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn mounts_outside_the_volume_are_refused() {
        let mut c = agent();
        c.mounts.push(Mount::host(Path::new("/etc"), "/x", true));
        assert!(pod_spec(&conf(), &c, &[]).is_err());
        let mut c = agent();
        c.mounts = vec![Mount::host(Path::new("/crucible/a/../../etc"), "/x", true)];
        assert!(pod_spec(&conf(), &c, &[]).is_err());
        let mut c = agent();
        c.network = Network::Container("serve".into());
        assert!(pod_spec(&conf(), &c, &[]).is_err());
    }

    #[test]
    fn restricted_pods_meet_the_standard() {
        let mut cf = conf();
        cf.restricted = true;
        cf.runtime_class = Some("gvisor".into());
        // arcbench-official's build container: root, three capabilities.
        let build = ContainerSpec {
            name: "crucible-build-1".into(),
            image: "crucible-scorer-arcbench-official:local".into(),
            network: Network::Named {
                name: "crucible-bnet-1".into(),
                aliases: vec![],
            },
            cap_drop: vec!["ALL".into()],
            cap_add: vec![
                "CHOWN".into(),
                "DAC_OVERRIDE".into(),
                "NET_BIND_SERVICE".into(),
            ],
            no_new_privileges: false,
            ..Default::default()
        };
        let p = pod_spec(&cf, &build, &[]).unwrap();
        let s = &p["spec"];
        let sc = &s["containers"][0]["securityContext"];
        assert_eq!(sc["runAsUser"], RESTRICTED_UID);
        assert_eq!(sc["runAsGroup"], RESTRICTED_UID);
        assert_eq!(sc["runAsNonRoot"], true);
        assert_eq!(sc["allowPrivilegeEscalation"], false);
        assert_eq!(
            sc["capabilities"],
            json!({"drop": ["ALL"], "add": ["NET_BIND_SERVICE"]})
        );
        assert_eq!(sc["seccompProfile"]["type"], "RuntimeDefault");
        assert_eq!(s["securityContext"]["runAsNonRoot"], true);
        assert_eq!(s["runtimeClassName"], "gvisor");
        // The netgate init container meets it too.
        let g = &s["initContainers"][0]["securityContext"];
        assert_eq!(g["runAsNonRoot"], true);
        assert_eq!(g["capabilities"]["drop"], json!(["ALL"]));
        // A non-root user is kept.
        let p = pod_spec(&cf, &agent(), &[]).unwrap();
        assert_eq!(
            p["spec"]["containers"][0]["securityContext"]["runAsUser"],
            1000
        );
        // baseline: the container's own flags, as before.
        let p = pod_spec(&conf(), &build, &[]).unwrap();
        let sc = &p["spec"]["containers"][0]["securityContext"];
        assert!(sc.get("runAsUser").is_none());
        assert_eq!(
            sc["capabilities"]["add"],
            json!(["CHOWN", "DAC_OVERRIDE", "NET_BIND_SERVICE"])
        );
        assert!(p["spec"].get("runtimeClassName").is_none());
    }

    #[test]
    fn a_shared_network_namespace_is_an_ephemeral_container() {
        // arcbench-official's test container: --network container:<app>.
        let t = ContainerSpec {
            name: "crucible-test-1".into(),
            image: "crucible-scorer-arcbench-official:local".into(),
            network: Network::Container("crucible-serve-1".into()),
            user: Some("1000:1000".into()),
            cap_drop: vec!["ALL".into()],
            no_new_privileges: true,
            limits: Limits {
                memory: Some("2g".into()),
                cpus: Some("2".into()),
                ..Default::default()
            },
            shm_size: Some("1g".into()),
            tmpfs: vec![Tmpfs {
                dst: "/workspace".into(),
                opts: "tmpfs-mode=1777".into(),
                via_mount: true,
            }],
            mounts: vec![
                Mount::host(Path::new("/crucible/tmp/t/tests"), "/pack", true),
                Mount::host(Path::new("/crucible/tmp/w/results"), "/results", false),
            ],
            env: vec![("HOME".into(), "/tmp".into())],
            entrypoint: Some(vec!["python3".into()]),
            args: vec!["/opt/crucible/official.py".into(), "test".into()],
            ..Default::default()
        };
        let (c, copies) = guest_spec(&conf(), &t).unwrap();
        assert_eq!(c["name"], "crucible-test-1");
        assert_eq!(
            c["image"],
            "10.43.200.200:5000/crucible-scorer-arcbench-official:local"
        );
        assert!(c.get("resources").is_none());
        // No subPath (not allowed for ephemeral containers): spare volumes.
        assert_eq!(
            c["volumeMounts"],
            json!([
                {"name": "guest-0", "mountPath": "/pack", "readOnly": true},
                {"name": "guest-1", "mountPath": "/results", "readOnly": false},
                {"name": "guest-2", "mountPath": "/workspace"},
            ])
        );
        assert_eq!(
            copies,
            [
                GuestCopy {
                    slot: 0,
                    sub: Some("tmp/t/tests".into()),
                    back: false
                },
                GuestCopy {
                    slot: 1,
                    sub: Some("tmp/w/results".into()),
                    back: true
                },
                GuestCopy {
                    slot: 2,
                    sub: None,
                    back: false
                },
            ]
        );
        // Every Pod has the spare volumes.
        let host = pod_spec(&conf(), &agent(), &[]).unwrap();
        let vols = host["spec"]["volumes"].as_array().unwrap();
        assert!(vols.contains(&json!({"name": "guest-3", "emptyDir": {}})));
        // The helper copies in every mount, back only the read-write one.
        let h = copy_helper("crucible-test-1-in", &copies, false).unwrap();
        let cmd = h["command"][2].as_str().unwrap();
        assert!(cmd.contains("cp -a '/data/tmp/t/tests'/. /g0/"), "{cmd}");
        assert!(cmd.contains("cp -a '/data/tmp/w/results'/. /g1/"), "{cmd}");
        assert_eq!(h["volumeMounts"][0]["readOnly"], true);
        assert_eq!(h["securityContext"]["runAsNonRoot"], true);
        let o = copy_helper("crucible-test-1-out", &copies, true).unwrap();
        let cmd = o["command"][2].as_str().unwrap();
        assert!(
            cmd.contains("cp -a /g1/. '/data/tmp/w/results'/") && !cmd.contains("/g0"),
            "{cmd}"
        );
        assert_eq!(o["volumeMounts"][0]["readOnly"], false);
        let mut many = t.clone();
        many.tmpfs = vec![many.tmpfs[0].clone(); 3];
        assert!(guest_spec(&conf(), &many).is_err());
        assert_eq!(c["securityContext"]["runAsUser"], 1000);
        assert_eq!(c["command"], json!(["python3"]));
        assert_eq!(
            guest_label("crucible-test-1"),
            "guest.crucible/crucible-test-1"
        );
        // Mounts outside the volume are refused as for any Pod.
        let mut bad = t.clone();
        bad.mounts = vec![Mount::host(Path::new("/etc"), "/x", true)];
        assert!(guest_spec(&conf(), &bad).is_err());

        // The host's resources grow by the guest's (API-normalised forms).
        let host = json!({"limits": {"memory": "2Gi", "cpu": "1"}, "requests": {"memory": "2Gi", "cpu": "1"}});
        let g = grown(&host, &t.limits).unwrap().unwrap();
        assert_eq!(g["limits"], json!({"memory": "4294967296", "cpu": "3000m"}));
        assert_eq!(g["requests"], g["limits"]);
        let host = json!({"limits": {"memory": "512Mi", "cpu": "500m"}});
        let g = grown(
            &host,
            &Limits {
                memory: Some("512m".into()),
                cpus: Some("0.5".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(g["limits"], json!({"memory": "1073741824", "cpu": "1000m"}));
        assert!(grown(&json!({}), &t.limits).unwrap().is_none());
        assert!(grown(&host, &Limits::default()).unwrap().is_none());
    }

    #[test]
    fn scorer_containers_map_to_pods() {
        // playwright's test runner: internal network, alias of the app,
        // shm, caller's uid, image built by the script.
        let c = ContainerSpec {
            name: "crucible-runner-17000".into(),
            image: "crucible-scorer-playwright:local".into(),
            user: Some("1000:1000".into()),
            network: Network::Named {
                name: "crucible-net-17000".into(),
                aliases: vec![],
            },
            limits: Limits {
                memory: Some("2g".into()),
                cpus: Some("2.0".into()),
                pids: Some(1024),
                ..Default::default()
            },
            cap_drop: vec!["ALL".into()],
            no_new_privileges: true,
            shm_size: Some("1g".into()),
            tmpfs: vec![Tmpfs {
                dst: "/tmp".into(),
                opts: "rw,noexec,size=64m".into(),
                via_mount: false,
            }],
            labels: vec![("crucible.scorer.run".into(), "17000".into())],
            ..Default::default()
        };
        let p = pod_spec(&conf(), &c, &[("app".into(), "10.42.0.20".into())]).unwrap();
        let s = &p["spec"];
        assert_eq!(
            s["hostAliases"],
            json!([{"ip": "10.42.0.20", "hostnames": ["app"]}])
        );
        assert_eq!(
            p["metadata"]["labels"]["net.crucible/crucible-net-17000"],
            "1"
        );
        assert_eq!(p["metadata"]["labels"]["crucible.scorer.run"], "17000");
        assert_eq!(
            s["containers"][0]["image"],
            "10.43.200.200:5000/crucible-scorer-playwright:local"
        );
        let vols = s["volumes"].as_array().unwrap();
        assert!(vols.contains(
            &json!({"name": "tmpfs-0", "emptyDir": {"medium": "Memory", "sizeLimit": "64Mi"}})
        ));
        assert!(vols.contains(
            &json!({"name": "shm", "emptyDir": {"medium": "Memory", "sizeLimit": "1Gi"}})
        ));
        // The app: its alias as a label (the next pods find it).
        let app = ContainerSpec {
            name: "crucible-app-17000".into(),
            image: "crucible-app-17000".into(),
            network: Network::Named {
                name: "crucible-net-17000".into(),
                aliases: vec!["app".into()],
            },
            cap_drop: vec!["NET_RAW".into(), "MKNOD".into()],
            ..Default::default()
        };
        let p = pod_spec(&conf(), &app, &[]).unwrap();
        assert_eq!(
            p["metadata"]["labels"]["alias.crucible/crucible-net-17000"],
            "app"
        );
        assert_eq!(
            p["spec"]["containers"][0]["securityContext"]["capabilities"]["drop"],
            json!(["NET_RAW", "MKNOD"])
        );
        assert_eq!(
            p["spec"]["containers"][0]["securityContext"]["allowPrivilegeEscalation"],
            true
        );
        // No network: no policy lets it out, no DNS.
        let none = ContainerSpec {
            name: "h".into(),
            image: PROBE_IMAGE.into(),
            network: Network::None,
            ..Default::default()
        };
        let p = pod_spec(&conf(), &none, &[]).unwrap();
        assert!(
            !p["metadata"]["labels"]
                .as_object()
                .unwrap()
                .keys()
                .any(|k| k.starts_with(NET_PREFIX) || k == INTERNET)
        );
        assert_eq!(p["spec"]["containers"][0]["image"], PROBE_IMAGE);
        assert_eq!(p["spec"]["dnsPolicy"], "None");
        assert_eq!(p["spec"]["initContainers"][0]["name"], "netgate");
        // The default network (internet) has no gate: it is not closed.
        let open = ContainerSpec {
            name: "proxy".into(),
            image: "crucible-scorer-x".into(),
            ..Default::default()
        };
        let p = pod_spec(&conf(), &open, &[]).unwrap();
        assert!(p["spec"].get("initContainers").is_none());
        assert_eq!(p["metadata"]["labels"][INTERNET], "1");
    }

    #[test]
    fn network_policies() {
        let n = internal_policy(
            "ns",
            &NetSpec {
                name: "crucible-net-1".into(),
                internal: true,
                labels: vec![],
            },
        );
        let sel = json!({"matchLabels": {"net.crucible/crucible-net-1": "1"}});
        assert_eq!(n["spec"]["podSelector"], sel);
        assert_eq!(n["spec"]["policyTypes"], json!(["Ingress", "Egress"]));
        assert_eq!(
            n["spec"]["ingress"],
            json!([{"from": [{"podSelector": sel}]}])
        );
        assert_eq!(n["spec"]["egress"], json!([{"to": [{"podSelector": sel}]}]));

        let [sbx, step] = sandbox_policies("ns", "crucible-sbx-a", "job-a", &[8787, 3128], "a");
        let member = json!({"matchLabels": {"net.crucible/crucible-sbx-a": "1"}});
        let step_sel = json!({"matchLabels": {"crucible/step": "job-a"}});
        let ports = json!([{"protocol": "TCP", "port": 8787}, {"protocol": "TCP", "port": 3128}]);
        assert_eq!(sbx["spec"]["podSelector"], member);
        // Members: nothing in, out only to the step's ports.
        assert_eq!(sbx["spec"]["policyTypes"], json!(["Ingress", "Egress"]));
        assert!(sbx["spec"].get("ingress").is_none());
        assert_eq!(
            sbx["spec"]["egress"],
            json!([{"to": [{"podSelector": step_sel}], "ports": ports}])
        );
        assert_eq!(step["spec"]["podSelector"], step_sel);
        assert_eq!(
            step["spec"]["ingress"],
            json!([{"from": [{"podSelector": member}], "ports": ports}])
        );
        assert_eq!(sbx["metadata"]["labels"]["crucible.run"], "a");

        let i = internet_policy("ns");
        let except = &i["spec"]["egress"][0]["to"][0]["ipBlock"]["except"];
        for c in [
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
        ] {
            assert!(except.as_array().unwrap().contains(&json!(c)), "{c}");
        }
    }

    #[test]
    fn probes_cover_the_list_and_any_success_fails() {
        let p = probes(&conf());
        let names: Vec<&str> = p.iter().map(|(n, _)| *n).collect();
        for want in [
            "dns_cluster",
            "dns_github",
            "tcp_1_1_1_1_443",
            "tcp_apiserver",
            "tcp_metadata",
            "tcp_node_kubelet",
            "tcp_registry",
        ] {
            assert!(names.contains(&want), "{want}");
        }
        let all_blocked: String = names
            .iter()
            .map(|n| format!("{n} blocked\n"))
            .collect::<String>()
            + "probes-done\n";
        assert!(probe_failures(&all_blocked, &p).unwrap().is_empty());
        let one = all_blocked.replace("tcp_metadata blocked", "tcp_metadata ok");
        assert_eq!(probe_failures(&one, &p).unwrap(), ["tcp_metadata"]);
        // A probe pod that died before the end refuses too.
        assert!(probe_failures("dns_cluster blocked\n", &p).is_err());
        assert!(probe_script(&p).contains("if nc -w 5 169.254.169.254 80"));
    }

    #[test]
    fn unnamed_containers_get_distinct_pod_names() {
        let c = ContainerSpec {
            image: "busybox".into(),
            labels: vec![(RUN_LABEL.into(), "local-x-score-r1".into())],
            network: Network::None,
            ..Default::default()
        };
        let a = pod_spec(&conf(), &c, &[]).unwrap();
        let b = pod_spec(&conf(), &c, &[]).unwrap();
        let (na, nb) = (
            a["metadata"]["name"].as_str().unwrap(),
            b["metadata"]["name"].as_str().unwrap(),
        );
        assert_ne!(na, nb);
        for (n, p) in [(na, &a), (nb, &b)] {
            assert!(n.starts_with("ctr-") && dns_name(n) == n, "{n}");
            assert_eq!(p["metadata"]["labels"][RUN_LABEL], "local-x-score-r1");
        }
        let named = ContainerSpec {
            name: "crucible-app".into(),
            ..c
        };
        assert_eq!(pod_name(&named), "crucible-app");
    }

    #[test]
    fn names_and_images() {
        assert_eq!(dns_name("crucible-app-17000"), "crucible-app-17000");
        let n = dns_name("Crucible_Stage.1");
        assert!(n.len() <= 63 && n.starts_with("crucible-stage-1-"), "{n}");
        assert_ne!(dns_name("a_b"), dns_name("a.b"));
        assert!(dns_name(&"x".repeat(100)).len() <= 63);
        assert_eq!(label_value("local-x-score-r1"), "local-x-score-r1");
        assert!(label_value(&"y".repeat(80)).len() <= 63);
        assert_eq!(
            map_image("r:5000", "crucible-app-1"),
            "r:5000/crucible-app-1"
        );
        assert_eq!(map_image("r:5000", "python:3.12"), "python:3.12");
        assert_eq!(
            map_image("r:5000", "r:5000/crucible-x:run"),
            "r:5000/crucible-x:run"
        );
        assert_eq!(
            registry_path("r:5000", "r:5000/crucible-x:run"),
            Some(("crucible-x".into(), "run".into()))
        );
        assert_eq!(registry_path("r:5000", "busybox"), None);
        assert_eq!(quantity("512m").unwrap(), "512Mi");
        assert_eq!(quantity("2g").unwrap(), "2Gi");
        assert_eq!(cpus("1.0").unwrap(), "1000m");
        assert_eq!(cpus("0.5").unwrap(), "500m");
    }

    #[test]
    fn builds_go_to_buildkit_and_the_registry() {
        let a = buildctl_args(
            &conf(),
            &BuildSpec {
                dir: "/crucible/tmp/app_src".into(),
                tag: "crucible-app-1".into(),
                no_network: true,
                ..Default::default()
            },
        );
        let s = a.join(" ");
        assert!(s.starts_with(
            "--addr tcp://buildkitd.crucible-system:1234 build --frontend dockerfile.v0"
        ));
        assert!(s.contains(
            "--local context=/crucible/tmp/app_src --local dockerfile=/crucible/tmp/app_src"
        ));
        assert!(s.contains("--opt force-network-mode=none"));
        assert!(s.ends_with("--output type=image,name=10.43.200.200:5000/crucible-app-1,push=true,registry.insecure=true"));
    }
}
