//! `crucible eval k8s`: the same driver as `eval local`, every step a
//! Kubernetes Job in the evaluation's own namespace (docs/kubernetes.md).
//! The steps start their containers as Pods there (the native backend,
//! [`crate::executor::k8s`]).
//!
//! Per evaluation the driver creates namespace `crucible-e-<eval id>` with:
//! a default-deny NetworkPolicy (and one letting step Pods out), a PVC for
//! the evaluation's files, mounted at `/crucible` in every Pod, the
//! service account the container-starting steps use (bound to ClusterRole
//! `crucible-step`: Pods and NetworkPolicies of this namespace only), and a
//! loader Pod through which the driver copies the evaluation directory in
//! and the results out (`tar` over `kubectl exec`). At the end the
//! namespace is deleted, with everything in it.
//!
//! Each step: a Secret holding exactly the step's secrets (its list,
//! [`StepSpec::secrets`]; anything else is refused, so `score-tests` can
//! only get the one-run key), a Job (`backoffLimit: 0`; retries are the
//! driver's) whose Pod mounts each secret as a file under
//! `/run/crucible/secrets`, and both deleted when the step ends.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::executor::k8s::{
    STEP_LABEL, dns_name, label_value, parse_selector, place, selector_string,
};
use crate::steps::{Pool, Secret, StepSpec};

#[derive(clap::Args)]
pub struct K8sArgs {
    #[command(flatten)]
    pub eval: crate::local::LocalArgs,
    /// The image the steps run in (deploy/k8s/step-image.yaml).
    #[arg(long, default_value = "10.43.200.200:5000/crucible-step:1")]
    pub step_image: String,
    /// The cluster registry (deploy/k8s/crucible-system.yaml).
    #[arg(long, default_value = "10.43.200.200:5000")]
    pub registry: String,
    #[arg(long, default_value = "tcp://buildkitd.crucible-system:1234")]
    pub buildkit: String,
    /// The cluster DNS service address (the network gate and probes).
    #[arg(long, default_value = "10.43.0.10")]
    pub cluster_dns: String,
    /// Size of the evaluation's volume.
    #[arg(long, default_value = "20Gi")]
    pub volume_size: String,
    /// Storage class of the evaluation's volume (default: the cluster's).
    #[arg(long)]
    pub storage_class: Option<String>,
    /// The volume is ReadWriteMany (a shared storage class: NFS, CephFS):
    /// steps and the Pods they start may run on any node of their pool.
    /// Without it the volume is ReadWriteOnce and a step's Pods run on the
    /// step's node.
    #[arg(long)]
    pub rwx: bool,
    /// Nodes of the steps that start no untrusted code (handoff, publish)
    /// and of the loader: `key=value[,key=value]` (default: any node).
    #[arg(long, default_value = "")]
    pub trusted_selector: String,
    /// Nodes of the steps that run agents or uploaded tests (generate,
    /// score-tests) and of every Pod they start (default: any node).
    #[arg(long, default_value = "")]
    pub sandbox_selector: String,
    /// The nodes limit processes per Pod (kubelet `pod-max-pids`).
    #[arg(long)]
    pub pids_limited: bool,
    /// The `crucible` binary copied to the volume (Linux, runs in the step
    /// image); default: this one.
    #[arg(long)]
    pub crucible_bin: Option<PathBuf>,
    /// Upper bound of one step, seconds.
    #[arg(long, default_value_t = 6 * 3600)]
    pub step_timeout_s: u64,
    /// The Pod Security Standard the evaluation's namespace enforces:
    /// `restricted` (every Pod non-root, no capabilities but
    /// NET_BIND_SERVICE, no privilege escalation, RuntimeDefault seccomp;
    /// containers asking for root run as uid 1000) or `baseline` (the
    /// containers' own flags, e.g. an app image that must run as root).
    #[arg(long, default_value = "restricted", value_parser = ["restricted", "baseline"])]
    pub pod_security: String,
    /// RuntimeClass of every Pod the steps start (agents, apps, tests,
    /// scorers): e.g. `gvisor` (runsc) or `kata`; it must exist.
    #[arg(long)]
    pub runtime_class: Option<String>,
}

/// Where the evaluation's volume is mounted in every Pod.
pub const ROOT: &str = "/crucible";
const SECRETS_DIR: &str = "/run/crucible/secrets";
const PVC: &str = "data";
const SA: &str = "crucible-step";
const LOADER: &str = "loader";
const UID: i64 = 1000;

/// The namespace of an evaluation.
pub fn namespace(eval_id: &str) -> String {
    dns_name(&format!("crucible-e-{eval_id}"))
}

/// The Job (and Secret) name of a step (`score-tests r1` -> `score-tests-r1`).
pub fn job_name(label: &str) -> String {
    dns_name(&format!("crucible-{}", label.replace(' ', "-")))
}

/// The Secret holding exactly `secrets`, all of them in `step`'s list.
pub fn secret_spec(
    ns: &str,
    name: &str,
    step: &StepSpec,
    secrets: &[(Secret, &[u8])],
) -> Result<Value> {
    let mut data = serde_json::Map::new();
    for (s, v) in secrets {
        if !step.secrets.contains(s) {
            bail!("step {} may not hold {}", step.name, s.name());
        }
        data.insert(
            s.name().to_owned(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(v)),
        );
    }
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": name, "namespace": ns},
        "type": "Opaque",
        "data": data,
    }))
}

/// Settings of the cluster the steps need.
pub struct Cluster {
    pub step_image: String,
    pub registry: String,
    pub buildkit: String,
    pub dns: String,
    pub pids_limited: bool,
    pub timeout_s: u64,
    /// The volume is ReadWriteMany: Pods need not share the step's node.
    pub rwx: bool,
    pub trusted: Vec<(String, String)>,
    pub sandbox: Vec<(String, String)>,
    /// `restricted` or `baseline` (the namespace's Pod Security level).
    pub pod_security: String,
    pub runtime_class: Option<String>,
}

/// The Job of one step: `crucible step <name> --secrets-dir ... <args>`
/// in the step image, one file per secret (all of them in the step's
/// list), the evaluation's volume at [`ROOT`]. Steps that start containers
/// get the service account and the executor's settings; the others no
/// Kubernetes credentials at all.
pub fn job_spec(
    ns: &str,
    name: &str,
    step: &StepSpec,
    exe: &str,
    args: &[String],
    secrets: &[Secret],
    c: &Cluster,
) -> Result<Value> {
    let mut mounts = vec![json!({"name": "data", "mountPath": ROOT})];
    for s in secrets {
        if !step.secrets.contains(s) {
            bail!("step {} may not hold {}", step.name, s.name());
        }
        mounts.push(json!({
            "name": "secrets",
            "mountPath": format!("{SECRETS_DIR}/{}", s.name()),
            "subPath": s.name(),
            "readOnly": true,
        }));
    }
    let tmp = format!("{ROOT}/tmp/{name}");
    let mut env = vec![
        json!({"name": "TMPDIR", "value": tmp}),
        json!({"name": "HOME", "value": tmp}),
    ];
    let sandbox = step.needs.containers;
    if sandbox {
        let field =
            |k: &str, f: &str| json!({"name": k, "valueFrom": {"fieldRef": {"fieldPath": f}}});
        for (k, v) in [
            ("CRUCIBLE_EXECUTOR", "k8s"),
            ("CRUCIBLE_K8S_NAMESPACE", ns),
            ("CRUCIBLE_K8S_ROOT", ROOT),
            ("CRUCIBLE_K8S_PVC", PVC),
            ("CRUCIBLE_K8S_STEP", name),
            ("CRUCIBLE_K8S_REGISTRY", &c.registry),
            ("CRUCIBLE_K8S_BUILDKIT", &c.buildkit),
            ("CRUCIBLE_K8S_DNS", &c.dns),
            (
                "CRUCIBLE_K8S_PIDS_LIMIT",
                if c.pids_limited { "1" } else { "0" },
            ),
            ("CRUCIBLE_K8S_SAME_NODE", if c.rwx { "0" } else { "1" }),
            ("CRUCIBLE_K8S_NODE_SELECTOR", &selector_string(&c.sandbox)),
            ("CRUCIBLE_K8S_POD_SECURITY", &c.pod_security),
            (
                "CRUCIBLE_K8S_RUNTIME_CLASS",
                c.runtime_class.as_deref().unwrap_or(""),
            ),
        ] {
            env.push(json!({"name": k, "value": v}));
        }
        env.push(field("POD_IP", "status.podIP"));
        env.push(field("HOST_IP", "status.hostIP"));
        env.push(field("NODE_NAME", "spec.nodeName"));
    }
    let mut command = vec![
        "bash".to_string(),
        "-c".into(),
        r#"mkdir -p "$TMPDIR" && exec "$0" "$@""#.into(),
        exe.to_owned(),
        "step".into(),
        step.name.into(),
        "--secrets-dir".into(),
        SECRETS_DIR.into(),
    ];
    command.extend(args.iter().cloned());
    let mut pod = json!({
        "restartPolicy": "Never",
        "automountServiceAccountToken": sandbox,
        "enableServiceLinks": false,
        "securityContext": {
            "runAsUser": UID, "runAsGroup": UID, "fsGroup": UID, "runAsNonRoot": true,
            "seccompProfile": {"type": "RuntimeDefault"},
        },
        "containers": [{
            "name": "step",
            "image": c.step_image,
            "imagePullPolicy": "IfNotPresent",
            "command": command,
            "env": env,
            "volumeMounts": mounts,
            "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}},
            "resources": {"requests": {"cpu": "500m", "memory": "512Mi"}},
        }],
        "volumes": [
            {"name": "data", "persistentVolumeClaim": {"claimName": PVC}},
            {"name": "secrets", "secret": {"secretName": name, "defaultMode": 0o440}},
        ],
    });
    if sandbox {
        pod["serviceAccountName"] = json!(SA);
    }
    let (pool, selector) = match step.needs.pool {
        Pool::Trusted => ("trusted", &c.trusted),
        Pool::Sandbox => ("sandbox", &c.sandbox),
    };
    place(&mut pod, selector);
    Ok(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {"name": name, "namespace": ns, "labels": {"crucible/step-name": step.name, "crucible/pool": pool}},
        "spec": {
            "backoffLimit": 0,
            "activeDeadlineSeconds": c.timeout_s,
            "template": {
                "metadata": {"labels": {"crucible/role": "step", STEP_LABEL: label_value(name)}},
                "spec": pod,
            },
        },
    }))
}

/// A node selector (`key=value` pairs).
pub type Selector = Vec<(String, String)>;

/// The pools of a cluster with more than one node, unless given.
pub const TRUSTED_POOL: &str = "crucible/pool=trusted";
pub const SANDBOX_POOL: &str = "crucible/pool=sandbox";

/// Node pools, from the cluster's nodes (`kubectl get nodes -o json`):
/// `(trusted, sandbox, what to log)`. One node: nothing to split, the
/// selectors stay as given and the log says so. More: trusted steps and
/// sandbox steps (with every Pod they start) must go to disjoint pools,
/// [`TRUSTED_POOL`] / [`SANDBOX_POOL`] unless given, each with at least
/// one node, every sandbox node tainted with its labels (`NoSchedule`, so
/// nothing else is scheduled there), and the volume must be ReadWriteMany
/// (steps of one evaluation then run on different nodes).
pub fn pools(
    nodes: &Value,
    trusted: &[(String, String)],
    sandbox: &[(String, String)],
    rwx: bool,
) -> Result<(Selector, Selector, String)> {
    let items = nodes["items"].as_array().cloned().unwrap_or_default();
    let name = |n: &Value| n["metadata"]["name"].as_str().unwrap_or("?").to_owned();
    if items.len() <= 1 {
        let n = items.first().map(name).unwrap_or_default();
        return Ok((
            trusted.to_vec(),
            sandbox.to_vec(),
            format!(
                "node pools: single node {n}: trusted and sandbox steps share it (pools cannot be split on one node)"
            ),
        ));
    }
    let pick = |given: &[(String, String)], default: &str| -> Selector {
        if given.is_empty() {
            parse_selector(default).expect("constant")
        } else {
            given.to_vec()
        }
    };
    let (t, s) = (pick(trusted, TRUSTED_POOL), pick(sandbox, SANDBOX_POOL));
    let matches = |n: &Value, sel: &[(String, String)]| {
        sel.iter()
            .all(|(k, v)| n["metadata"]["labels"][k].as_str() == Some(v.as_str()))
    };
    let (tn, sn): (Vec<&Value>, Vec<&Value>) = (
        items.iter().filter(|n| matches(n, &t)).collect(),
        items.iter().filter(|n| matches(n, &s)).collect(),
    );
    let fix = format!(
        "label the nodes (kubectl label node <node> {} / {}), taint the sandbox nodes \
         (kubectl taint node <node> {}:NoSchedule), see docs/kubernetes.md",
        selector_string(&t),
        selector_string(&s),
        selector_string(&s)
    );
    if tn.is_empty() || sn.is_empty() {
        bail!(
            "{} nodes but no {} pool ({}): trusted and sandbox steps must run on separate nodes; {fix}",
            items.len(),
            if tn.is_empty() { "trusted" } else { "sandbox" },
            selector_string(if tn.is_empty() { &t } else { &s })
        );
    }
    if let Some(n) = tn.iter().find(|n| matches(n, &s)) {
        bail!("node {} is in both pools; {fix}", name(n));
    }
    for n in &sn {
        let taints = n["spec"]["taints"].as_array().cloned().unwrap_or_default();
        let tainted = s.iter().all(|(k, v)| {
            taints.iter().any(|x| {
                x["key"] == k.as_str()
                    && x["value"].as_str().unwrap_or("") == v
                    && x["effect"] == "NoSchedule"
            })
        });
        if !tainted {
            bail!(
                "sandbox node {} is not tainted {}:NoSchedule (other Pods could land next to untrusted code); {fix}",
                name(n),
                selector_string(&s)
            );
        }
    }
    if !rwx {
        bail!(
            "{} nodes: trusted and sandbox steps run on different nodes, so the evaluation's volume must be \
             ReadWriteMany: --rwx --storage-class <shared class> (docs/kubernetes.md)",
            items.len()
        );
    }
    let names = |v: &[&Value]| v.iter().map(|n| name(n)).collect::<Vec<_>>().join(",");
    Ok((
        t.clone(),
        s.clone(),
        format!(
            "node pools: trusted {} = [{}], sandbox {} = [{}]",
            selector_string(&t),
            names(&tn),
            selector_string(&s),
            names(&sn)
        ),
    ))
}

/// What the namespace is made with.
pub struct NsOpts {
    pub volume_size: String,
    pub storage_class: Option<String>,
    pub step_image: String,
    pub rwx: bool,
    /// The loader's nodes (the trusted pool).
    pub trusted: Vec<(String, String)>,
    /// The Pod Security level the namespace enforces.
    pub pod_security: String,
}

/// The namespace's fixed objects: default deny, step Pods out, the
/// volume, the steps' service account and its binding, the loader Pod.
pub fn namespace_objects(ns: &str, eval_id: &str, a: &NsOpts) -> Vec<Value> {
    let mut pvc = json!({
        "apiVersion": "v1", "kind": "PersistentVolumeClaim",
        "metadata": {"name": PVC, "namespace": ns},
        "spec": {"accessModes": [if a.rwx { "ReadWriteMany" } else { "ReadWriteOnce" }],
                 "resources": {"requests": {"storage": a.volume_size}}},
    });
    if let Some(sc) = &a.storage_class {
        pvc["spec"]["storageClassName"] = json!(sc);
    }
    let mut loader = json!({
        "automountServiceAccountToken": false,
        "enableServiceLinks": false,
        "securityContext": {"runAsUser": UID, "runAsGroup": UID, "fsGroup": UID, "runAsNonRoot": true,
                            "seccompProfile": {"type": "RuntimeDefault"}},
        "containers": [{"name": "loader", "image": a.step_image, "command": ["sleep", "infinity"],
                        "securityContext": {"allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]}},
                        "volumeMounts": [{"name": "data", "mountPath": ROOT}]}],
        "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": PVC}}],
    });
    place(&mut loader, &a.trusted);
    vec![
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns, "labels": {
            "crucible/eval": label_value(eval_id),
            "pod-security.kubernetes.io/enforce": a.pod_security,
            "pod-security.kubernetes.io/enforce-version": "latest",
            "pod-security.kubernetes.io/warn": a.pod_security,
            "pod-security.kubernetes.io/audit": a.pod_security,
        }}}),
        json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
               "metadata": {"name": "default-deny", "namespace": ns},
               "spec": {"podSelector": {}, "policyTypes": ["Ingress", "Egress"]}}),
        json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
               "metadata": {"name": "steps-out", "namespace": ns},
               "spec": {"podSelector": {"matchLabels": {"crucible/role": "step"}},
                        "policyTypes": ["Egress"], "egress": [{}]}}),
        pvc,
        json!({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": SA, "namespace": ns},
               "automountServiceAccountToken": false}),
        json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
               "metadata": {"name": SA, "namespace": ns},
               "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": "crucible-step"},
               "subjects": [{"kind": "ServiceAccount", "name": SA, "namespace": ns}]}),
        json!({"apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": LOADER, "namespace": ns, "labels": {"crucible/role": "loader"}},
        "spec": loader}),
    ]
}

async fn kubectl(args: &[&str], stdin: Option<&[u8]>) -> Result<String> {
    use tokio::io::AsyncWriteExt;
    let mut last = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let mut child = tokio::process::Command::new("kubectl")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running kubectl")?;
        if let (Some(d), Some(mut w)) = (stdin, child.stdin.take()) {
            w.write_all(d).await?;
        }
        let out = child.wait_with_output().await?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        // Not worth retrying: the request itself is wrong.
        let permanent = err.contains("NotFound")
            || err.contains("AlreadyExists")
            || err.contains("Invalid")
            || err.contains("Forbidden");
        last = Some(anyhow!(
            "kubectl {}: {}",
            args.first().unwrap_or(&""),
            err.chars().take(400).collect::<String>()
        ));
        if permanent {
            break;
        }
    }
    Err(last.expect("attempted"))
}

/// One evaluation's namespace.
pub struct K8s {
    pub ns: String,
    pub cluster: Cluster,
    eval_id: String,
    opts: NsOpts,
    ready: bool,
}

/// How a step Job ended (times: its Pod's container).
pub struct Ended {
    pub ok: bool,
    pub started: SystemTime,
    pub ended: SystemTime,
}

impl K8s {
    /// The settings; [`K8s::bind`] names the evaluation.
    pub fn new(a: &K8sArgs) -> Result<K8s> {
        let trusted = parse_selector(&a.trusted_selector).context("--trusted-selector")?;
        let sandbox = parse_selector(&a.sandbox_selector).context("--sandbox-selector")?;
        Ok(K8s {
            ns: String::new(),
            cluster: Cluster {
                step_image: a.step_image.clone(),
                registry: a.registry.clone(),
                buildkit: a.buildkit.clone(),
                dns: a.cluster_dns.clone(),
                pids_limited: a.pids_limited,
                timeout_s: a.step_timeout_s,
                rwx: a.rwx,
                trusted: trusted.clone(),
                sandbox,
                pod_security: a.pod_security.clone(),
                runtime_class: a.runtime_class.clone().filter(|r| !r.is_empty()),
            },
            eval_id: String::new(),
            opts: NsOpts {
                volume_size: a.volume_size.clone(),
                storage_class: a.storage_class.clone(),
                step_image: a.step_image.clone(),
                rwx: a.rwx,
                trusted,
                pod_security: a.pod_security.clone(),
            },
            ready: false,
        })
    }

    pub fn bind(&mut self, eval_id: &str) {
        self.eval_id = eval_id.to_owned();
        self.ns = namespace(eval_id);
    }

    /// Create the namespace and copy `dir` (the evaluation directory) to
    /// its volume; once.
    pub async fn prepare(&mut self, dir: &Path) -> Result<()> {
        if self.ready {
            return Ok(());
        }
        self.check_cluster().await?;
        for o in namespace_objects(&self.ns, &self.eval_id, &self.opts) {
            kubectl(&["create", "-f", "-"], Some(o.to_string().as_bytes()))
                .await
                .with_context(|| format!("creating {} {}", o["kind"], o["metadata"]["name"]))?;
        }
        eprintln!("namespace {}: waiting for the loader pod", self.ns);
        kubectl(
            &[
                "-n",
                &self.ns,
                "wait",
                "--for=condition=Ready",
                &format!("pod/{LOADER}"),
                "--timeout=900s",
            ],
            None,
        )
        .await?;
        let tar = tokio::process::Command::new("tar")
            .arg("-C")
            .arg(dir)
            .args(["-cf", "-", "."])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let data = tar.wait_with_output().await?;
        if !data.status.success() {
            bail!("packing {}", dir.display());
        }
        kubectl(
            &[
                "-n",
                &self.ns,
                "exec",
                "-i",
                LOADER,
                "--",
                "tar",
                "-xf",
                "-",
                "-C",
                ROOT,
                "--no-same-owner",
                "--no-overwrite-dir",
                "-m",
            ],
            Some(&data.stdout),
        )
        .await
        .context("copying the evaluation to its volume")?;
        self.ready = true;
        Ok(())
    }

    /// Node pools ([`pools`]) and the RuntimeClass, before anything is
    /// created.
    async fn check_cluster(&mut self) -> Result<()> {
        let nodes: Value = serde_json::from_str(
            &kubectl(&["get", "nodes", "-o", "json"], None)
                .await
                .context("listing the cluster's nodes")?,
        )?;
        let (t, s, note) = pools(
            &nodes,
            &self.cluster.trusted,
            &self.cluster.sandbox,
            self.cluster.rwx,
        )?;
        eprintln!("{note}");
        self.cluster.trusted = t.clone();
        self.cluster.sandbox = s;
        self.opts.trusted = t;
        if let Some(rc) = &self.cluster.runtime_class {
            kubectl(&["get", "runtimeclass", rc, "-o", "name"], None)
                .await
                .with_context(|| format!("--runtime-class {rc}: no such RuntimeClass"))?;
            eprintln!("runtime class {rc}: every Pod the steps start");
        }
        eprintln!(
            "pod security: namespace enforces {}",
            self.cluster.pod_security
        );
        Ok(())
    }

    /// Copy `what` (paths relative to the volume) back into `dir`.
    pub async fn fetch(&self, dir: &Path, what: &[&str]) -> Result<()> {
        if !self.ready {
            return Ok(());
        }
        let mut args = vec![
            "-n",
            &self.ns,
            "exec",
            LOADER,
            "--",
            "tar",
            "-cf",
            "-",
            "-C",
            ROOT,
            "--ignore-failed-read",
        ];
        args.extend(what.iter().copied());
        let out = tokio::process::Command::new("kubectl")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await?;
        let mut tar = tokio::process::Command::new("tar")
            .arg("-C")
            .arg(dir)
            .args(["-xf", "-"])
            .stdin(Stdio::piped())
            .spawn()?;
        if let Some(mut w) = tar.stdin.take() {
            use tokio::io::AsyncWriteExt;
            w.write_all(&out.stdout).await?;
        }
        if !tar.wait().await?.success() {
            bail!("unpacking the results into {}", dir.display());
        }
        Ok(())
    }

    /// Delete the namespace and everything in it (volume included).
    pub async fn delete(&self) {
        if let Err(e) = kubectl(
            &[
                "delete",
                "namespace",
                &self.ns,
                "--ignore-not-found",
                "--wait=false",
            ],
            None,
        )
        .await
        {
            eprintln!("warning: deleting namespace {}: {e}", self.ns);
        }
    }

    /// Run one step as a Job: Secret, Job, its output, then delete both
    /// whatever happened.
    pub async fn run_step(
        &self,
        label: &str,
        step: &'static StepSpec,
        exe: &str,
        args: &[String],
        secrets: &[(Secret, &[u8])],
    ) -> Result<Ended> {
        let name = job_name(label);
        let names: Vec<Secret> = secrets.iter().map(|(s, _)| *s).collect();
        let job = job_spec(&self.ns, &name, step, exe, args, &names, &self.cluster)?;
        let secret = secret_spec(&self.ns, &name, step, secrets)?;
        kubectl(&["create", "-f", "-"], Some(secret.to_string().as_bytes()))
            .await
            .context("creating the step's secret")?;
        let r = async {
            kubectl(&["create", "-f", "-"], Some(job.to_string().as_bytes()))
                .await
                .context("creating the step job")?;
            eprintln!("   job {}/{name}", self.ns);
            self.wait(&name).await
        }
        .await;
        for what in ["job", "secret"] {
            if let Err(e) = kubectl(
                &[
                    "-n",
                    &self.ns,
                    "delete",
                    what,
                    &name,
                    "--ignore-not-found",
                    "--cascade=foreground",
                    "--wait=false",
                ],
                None,
            )
            .await
            {
                eprintln!("   warning: deleting {what} {name}: {e}");
            }
        }
        r
    }

    /// Follow the Job's Pod: its output to stderr while it runs, then how
    /// it ended.
    async fn wait(&self, job: &str) -> Result<Ended> {
        let sel = format!("job-name={job}");
        let mut following = false;
        let mut follower: Option<tokio::process::Child> = None;
        loop {
            let pods: Value = serde_json::from_str(
                &kubectl(
                    &["-n", &self.ns, "get", "pods", "-l", &sel, "-o", "json"],
                    None,
                )
                .await?,
            )?;
            let pod = pods["items"].as_array().and_then(|a| a.first()).cloned();
            if let Some(p) = &pod {
                let cs = &p["status"]["containerStatuses"][0]["state"];
                if !following && (cs["running"].is_object() || cs["terminated"].is_object()) {
                    following = true;
                    follower = tokio::process::Command::new("kubectl")
                        .args(["-n", &self.ns, "logs", "-f", &format!("job/{job}")])
                        .stdin(Stdio::null())
                        .stdout(Stdio::from(std::io::stderr()))
                        .spawn()
                        .ok();
                }
                if let Some(t) = cs["terminated"].as_object() {
                    if let Some(mut f) = follower.take() {
                        let _ = tokio::time::timeout(Duration::from_secs(30), f.wait()).await;
                    }
                    let time = |k: &str| {
                        t.get(k)
                            .and_then(Value::as_str)
                            .and_then(|s| humantime::parse_rfc3339(s).ok())
                            .unwrap_or_else(SystemTime::now)
                    };
                    let code = t.get("exitCode").and_then(Value::as_i64);
                    if code != Some(0) {
                        eprintln!(
                            "   step pod: {} (exit {code:?})",
                            t.get("reason").and_then(Value::as_str).unwrap_or("")
                        );
                    }
                    return Ok(Ended {
                        ok: code == Some(0),
                        started: time("startedAt"),
                        ended: time("finishedAt"),
                    });
                }
                if p["status"]["phase"] == "Failed" {
                    eprintln!("   step pod failed: {}", p["status"]["reason"]);
                    let now = SystemTime::now();
                    return Ok(Ended {
                        ok: false,
                        started: now,
                        ended: now,
                    });
                }
            } else {
                // No pod (yet): did the job itself fail (deadline)?
                let j: Value = serde_json::from_str(
                    &kubectl(&["-n", &self.ns, "get", "job", job, "-o", "json"], None).await?,
                )?;
                if j["status"]["failed"].as_i64().unwrap_or(0) > 0 {
                    let now = SystemTime::now();
                    return Ok(Ended {
                        ok: false,
                        started: now,
                        ended: now,
                    });
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steps::{STEPS, spec};

    fn cluster() -> Cluster {
        Cluster {
            step_image: "r/crucible-step:1".into(),
            registry: "r".into(),
            buildkit: "tcp://b:1234".into(),
            dns: "10.43.0.10".into(),
            pids_limited: true,
            timeout_s: 3600,
            rwx: false,
            trusted: vec![],
            sandbox: vec![],
            pod_security: "restricted".into(),
            runtime_class: None,
        }
    }

    fn job(step: &str, secrets: &[Secret]) -> Result<Value> {
        job_spec(
            "crucible-e-x",
            "crucible-x",
            spec(step),
            "/crucible/bin/crucible",
            &["--out".into(), "/crucible/x".into()],
            secrets,
            &cluster(),
        )
    }

    fn mounted(j: &Value) -> Vec<String> {
        j["spec"]["template"]["spec"]["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["name"] == "secrets")
            .map(|m| m["mountPath"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn score_tests_gets_only_the_run_key() {
        let j = job("score-tests", &[Secret::RunKey]).unwrap();
        assert_eq!(mounted(&j), ["/run/crucible/secrets/run-key"]);
        let text = j.to_string();
        for s in Secret::ALL.into_iter().filter(|s| *s != Secret::RunKey) {
            assert!(!text.contains(s.name()), "{} in score-tests job", s.name());
            assert!(job("score-tests", &[s]).is_err());
            assert!(secret_spec("ns", "n", spec("score-tests"), &[(s, b"v")]).is_err());
        }
        let sec = secret_spec("ns", "n", spec("score-tests"), &[(Secret::RunKey, b"k")]).unwrap();
        assert_eq!(sec["data"].as_object().unwrap().len(), 1);
        assert_eq!(sec["data"]["run-key"], "aw==");
    }

    #[test]
    fn every_step_mounts_exactly_what_it_is_given_from_its_list() {
        for st in STEPS {
            let j = job(st.name, st.secrets).unwrap();
            let want: Vec<String> = st
                .secrets
                .iter()
                .map(|s| format!("/run/crucible/secrets/{}", s.name()))
                .collect();
            assert_eq!(mounted(&j), want, "{}", st.name);
            for s in Secret::ALL.into_iter().filter(|s| !st.secrets.contains(s)) {
                assert!(job(st.name, &[s]).is_err(), "{} took {}", st.name, s.name());
            }
        }
    }

    #[test]
    fn jobs_run_once_and_only_container_steps_reach_the_api() {
        let g = job("generate", &[Secret::PlatformKey, Secret::DevModelCred]).unwrap();
        assert_eq!(g["spec"]["backoffLimit"], 0);
        assert_eq!(g["spec"]["activeDeadlineSeconds"], 3600);
        let p = &g["spec"]["template"]["spec"];
        assert_eq!(p["restartPolicy"], "Never");
        assert_eq!(p["serviceAccountName"], "crucible-step");
        assert_eq!(p["automountServiceAccountToken"], true);
        assert_eq!(p["securityContext"]["runAsNonRoot"], true);
        assert_eq!(
            g["spec"]["template"]["metadata"]["labels"]["crucible/step"],
            "crucible-x"
        );
        let c = &p["containers"][0];
        assert_eq!(c["securityContext"]["allowPrivilegeEscalation"], false);
        let cmd: Vec<&str> = c["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            cmd[3..],
            [
                "/crucible/bin/crucible",
                "step",
                "generate",
                "--secrets-dir",
                "/run/crucible/secrets",
                "--out",
                "/crucible/x"
            ]
        );
        let env = c["env"].to_string();
        assert!(env.contains("CRUCIBLE_EXECUTOR") && env.contains("status.podIP"));
        assert!(env.contains(r#"{"name":"CRUCIBLE_K8S_POD_SECURITY","value":"restricted"}"#));
        // Trusted steps: no service account token, no executor.
        let h = job("handoff", &[Secret::PlatformKey]).unwrap();
        let p = &h["spec"]["template"]["spec"];
        assert_eq!(p["automountServiceAccountToken"], false);
        assert!(p.get("serviceAccountName").is_none());
        assert!(
            !p["containers"][0]["env"]
                .to_string()
                .contains("CRUCIBLE_EXECUTOR")
        );
    }

    #[test]
    fn steps_go_to_their_pool() {
        // Default: no pools, a ReadWriteOnce volume, Pods on the step's node.
        let g = job("generate", &[]).unwrap();
        let p = &g["spec"]["template"]["spec"];
        assert!(p.get("nodeSelector").is_none() && p.get("tolerations").is_none());
        let env = p["containers"][0]["env"].to_string();
        assert!(env.contains(r#"{"name":"CRUCIBLE_K8S_SAME_NODE","value":"1"}"#));
        let o = NsOpts {
            volume_size: "1Gi".into(),
            storage_class: None,
            step_image: "i".into(),
            rwx: false,
            trusted: vec![],
            pod_security: "restricted".into(),
        };
        let objs = namespace_objects("ns", "e", &o);
        let ns = &objs[0]["metadata"]["labels"];
        assert_eq!(ns["pod-security.kubernetes.io/enforce"], "restricted");
        assert_eq!(ns["pod-security.kubernetes.io/enforce-version"], "latest");
        let pvc = objs
            .iter()
            .find(|o| o["kind"] == "PersistentVolumeClaim")
            .unwrap();
        assert_eq!(pvc["spec"]["accessModes"], json!(["ReadWriteOnce"]));

        // Multi-node: shared volume, two pools.
        let mut c = cluster();
        c.rwx = true;
        c.trusted = parse_selector("crucible/pool=trusted").unwrap();
        c.sandbox = parse_selector("crucible/pool=sandbox").unwrap();
        let on = |step: &str, secrets: &[Secret]| {
            job_spec("ns", "j", spec(step), "/c", &[], secrets, &c).unwrap()["spec"]["template"]
                ["spec"]
                .clone()
        };
        for (step, pool) in [
            ("generate", "sandbox"),
            ("score-tests", "sandbox"),
            ("handoff", "trusted"),
            ("publish", "trusted"),
        ] {
            let p = on(step, &[]);
            assert_eq!(p["nodeSelector"], json!({"crucible/pool": pool}), "{step}");
            assert_eq!(p["tolerations"][0]["value"], pool, "{step}");
        }
        let env = on("generate", &[])["containers"][0]["env"].to_string();
        assert!(env.contains(r#"{"name":"CRUCIBLE_K8S_SAME_NODE","value":"0"}"#));
        assert!(
            env.contains(
                r#"{"name":"CRUCIBLE_K8S_NODE_SELECTOR","value":"crucible/pool=sandbox"}"#
            )
        );
        let o = NsOpts {
            rwx: true,
            trusted: c.trusted.clone(),
            ..o
        };
        let objs = namespace_objects("ns", "e", &o);
        let pvc = objs
            .iter()
            .find(|o| o["kind"] == "PersistentVolumeClaim")
            .unwrap();
        assert_eq!(pvc["spec"]["accessModes"], json!(["ReadWriteMany"]));
        let loader = objs.iter().find(|o| o["kind"] == "Pod").unwrap();
        assert_eq!(
            loader["spec"]["nodeSelector"],
            json!({"crucible/pool": "trusted"})
        );
    }

    fn node(name: &str, labels: Value, taints: Value) -> Value {
        json!({"metadata": {"name": name, "labels": labels}, "spec": {"taints": taints}})
    }

    #[test]
    fn several_nodes_need_separate_tainted_pools() {
        let none: Vec<(String, String)> = vec![];
        // One node: nothing to split, the selectors as given.
        let one = json!({"items": [node("n1", json!({}), json!([]))]});
        let (t, s, note) = pools(&one, &none, &none, false).unwrap();
        assert!(t.is_empty() && s.is_empty() && note.contains("single node n1"));

        let taint = json!([{"key": "crucible/pool", "value": "sandbox", "effect": "NoSchedule"}]);
        let good = json!({"items": [
            node("srv", json!({"crucible/pool": "trusted"}), json!([])),
            node("a0", json!({"crucible/pool": "sandbox"}), taint.clone()),
        ]});
        let (t, s, note) = pools(&good, &none, &none, true).unwrap();
        assert_eq!(selector_string(&t), TRUSTED_POOL);
        assert_eq!(selector_string(&s), SANDBOX_POOL);
        assert!(note.contains("[srv]") && note.contains("[a0]"), "{note}");
        // A ReadWriteOnce volume cannot follow steps across nodes.
        assert!(pools(&good, &none, &none, false).is_err());
        // No sandbox pool, an untainted sandbox node, a node in both.
        let unlabeled =
            json!({"items": [node("a", json!({}), json!([])), node("b", json!({}), json!([]))]});
        assert!(pools(&unlabeled, &none, &none, true).is_err());
        let untainted = json!({"items": [
            node("srv", json!({"crucible/pool": "trusted"}), json!([])),
            node("a0", json!({"crucible/pool": "sandbox"}), json!([])),
        ]});
        let e = pools(&untainted, &none, &none, true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not tainted"), "{e}");
        let both = parse_selector("zone=a").unwrap();
        let zoned = json!({"items": [
            node("x", json!({"zone": "a"}), taint.clone()),
            node("y", json!({"zone": "a"}), taint),
        ]});
        assert!(pools(&zoned, &both, &both, true).is_err());
    }

    #[test]
    fn names_are_per_evaluation() {
        assert_eq!(namespace("k8s-20261004-abc"), "crucible-e-k8s-20261004-abc");
        assert_ne!(namespace("a-1234567"), namespace("a-1234568"));
        assert_eq!(job_name("score-tests r2"), "crucible-score-tests-r2");
        assert!(namespace(&"x".repeat(64)).len() <= 63);
    }
}
