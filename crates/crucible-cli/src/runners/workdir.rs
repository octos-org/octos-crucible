//! The `workdir` runner (docs/plugins.md §4.1): the agent works in a shared
//! work dir, stage by stage; at the end of a stage the stage's packager
//! turns the work dir into the stage output.
//!
//! The model credential arrives once on stdin and stays in this process; for
//! every stage a fresh in-process meter (own `usage.jsonl`) and egress proxy
//! are started on the sandbox bridge, the agent container runs with the
//! stage's inputs read-only at `/req`, the shared work dir at `/work` and a
//! shared `HOME`, and is stopped at the stage's time limit. The work dir is
//! snapshotted (packaged) every `--snapshot-interval-s`; at the end of a
//! stage it is packaged as the stage checkpoint (the last snapshot when the agent had
//! to be stopped).
//!
//! Output layout (read by `crucible report`):
//!
//! ```text
//! <out>/<stage>/usage.jsonl    meter log of the stage
//! <out>/<stage>/egress.jsonl   egress proxy log of the stage
//! <out>/<stage>/timing.json    wall clock and how the stage ended
//! <out>/<stage>/checkpoint.zip packaged output
//! <out>/<stage>/agent.log      container stdout+stderr
//! ```
//!
//! Only progress and numbers are printed (stderr); agent output goes to
//! `agent.log` only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use crucible_core::taskset::Stage;
use crucible_core::{AgentSpec, UsageRecord};
use crucible_meter::{Credential, Limits, MeterConfig, Upstream};
use crucible_metering::{Price, Pricing};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::executor::Executor;
use crate::plan::Budget;

#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Agent image built by `crucible build`.
    #[arg(long)]
    pub image: String,
    /// The package's agent.json.
    #[arg(long)]
    pub agent_json: PathBuf,
    #[arg(long)]
    pub taskset: PathBuf,
    /// Unpacked inputs, `<dir>/<stage id>/` (from `crucible taskset inputs`).
    #[arg(long)]
    pub inputs_dir: PathBuf,
    /// Run only the first N stages (development).
    #[arg(long)]
    pub stages: Option<usize>,
    #[arg(long)]
    pub model: String,
    /// Replica result dir: `<out>/<stage>/...`.
    #[arg(long)]
    pub out_dir: PathBuf,
    /// Scratch dir for the work dir, HOME and snapshots (kept across stages).
    #[arg(long)]
    pub scratch_dir: PathBuf,
    #[arg(long)]
    pub pricing: PathBuf,
    /// Egress allowlist (config/egress.json).
    #[arg(long)]
    pub egress_allow: PathBuf,
    /// Address the meter and egress proxy listen on (the sandbox bridge).
    #[arg(long, default_value = "172.31.250.1")]
    pub bind: String,
    /// Address the container uses to reach them; default: --bind.
    #[arg(long)]
    pub agent_host: Option<String>,
    #[arg(long, default_value_t = 8787)]
    pub meter_port: u16,
    #[arg(long, default_value_t = 3128)]
    pub egress_port: u16,
    /// Docker network of the sandbox (tools/sandbox-net.sh).
    #[arg(long, default_value = "crucible-sbx")]
    pub network: String,
    /// Run-wide caps, JSON as accepted by `crucible plan`.
    #[arg(long, default_value = "")]
    pub budget: String,
    #[arg(long, default_value_t = 900)]
    pub snapshot_interval_s: u64,
    /// Seconds between SIGTERM and SIGKILL when a stage is stopped.
    #[arg(long, default_value_t = 30)]
    pub grace_s: u64,
    /// uid:gid for the container; default: this process's.
    #[arg(long)]
    pub user: Option<String>,
    /// Label value `crucible.run=<this>` on every container of the run, so
    /// whoever started it can remove exactly its containers.
    #[arg(long, default_value = "")]
    pub run_label: String,
}

/// `timing.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    pub wall_s: f64,
    pub started_at: String,
    pub ended_at: String,
    pub time_limit_s: u64,
    /// `exited`, `deadline` or `aborted`.
    pub ended: String,
    pub exit_code: Option<i64>,
    /// `final`, `snapshot` or `none`.
    pub checkpoint_source: String,
    pub checkpoint_bytes: Option<u64>,
    pub snapshots: u32,
}

/// What the container gets; see docs/agent-contract.md.
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub network: String,
    pub user: String,
    pub req: PathBuf,
    pub work: PathBuf,
    pub home: PathBuf,
    pub agent_host: String,
    pub meter_port: u16,
    pub egress_port: u16,
    pub model: String,
    pub deadline_s: u64,
    pub entrypoint: Option<Vec<String>>,
    /// `crucible.run=<label>`.
    pub label: String,
}

pub const CONTAINER_HOME: &str = "/home/agent";

/// The backend-neutral container of an agent stage.
pub fn agent_container(c: &ContainerSpec) -> crate::executor::ContainerSpec {
    let meter = format!("http://{}:{}/v1", c.agent_host, c.meter_port);
    let egress = format!("http://{}:{}", c.agent_host, c.egress_port);
    let no_proxy = format!("{},localhost,127.0.0.1", c.agent_host);
    let mount = crate::executor::Mount::host;
    let env = [
        ("REQ_DIR", "/req".to_string()),
        ("WORK_DIR", "/work".to_string()),
        ("HOME", CONTAINER_HOME.to_string()),
        ("OPENAI_BASE_URL", meter),
        ("OPENAI_API_KEY", "dummy".to_string()),
        ("MODEL", c.model.clone()),
        ("DEADLINE_S", c.deadline_s.to_string()),
        ("HTTP_PROXY", egress.clone()),
        ("HTTPS_PROXY", egress.clone()),
        ("http_proxy", egress.clone()),
        ("https_proxy", egress),
        ("NO_PROXY", no_proxy.clone()),
        ("no_proxy", no_proxy),
    ];
    crate::executor::ContainerSpec {
        name: c.name.clone(),
        image: c.image.clone(),
        entrypoint: c.entrypoint.clone(),
        env: env.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        user: Some(c.user.clone()),
        limits: crate::executor::Limits {
            memory: Some("2g".into()),
            memory_swap: Some("2g".into()),
            cpus: Some("1".into()),
            pids: Some(1024),
        },
        network: crate::executor::Network::Named {
            name: c.network.clone(),
            aliases: vec![],
        },
        dns: Some("127.0.0.1".into()),
        mounts: vec![
            mount(&c.req, "/req", true),
            mount(&c.work, "/work", false),
            mount(&c.home, CONTAINER_HOME, false),
        ],
        workdir: Some("/work".into()),
        labels: vec![(crate::executor::RUN_LABEL.into(), c.label.clone())],
        init: true,
        cap_drop: vec!["ALL".into()],
        no_new_privileges: true,
        ..Default::default()
    }
}

/// `docker run` arguments of an agent stage's container.
#[cfg(test)]
pub fn docker_run_args(c: &ContainerSpec) -> Vec<String> {
    crate::executor::docker::DockerExecutor::run_args(&agent_container(c))
}

/// Usage so far, as the meter's budget counts it.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Used {
    pub requests: u64,
    pub tokens: u64,
    pub cost: f64,
}

pub fn used(records: &[UsageRecord]) -> Used {
    let mut u = Used::default();
    for r in records {
        if r.model_rejected || r.budget_exceeded.is_some() {
            continue;
        }
        u.requests += 1;
        u.tokens += r.prompt_tokens.unwrap_or(0) + r.completion_tokens.unwrap_or(0);
        u.cost += r.cost_usd.unwrap_or(0.0);
    }
    u
}

/// The caps left for the next stage once `used` is spent.
pub fn remaining(budget: &Budget, used: Used) -> Limits {
    Limits {
        max_requests: budget.max_requests.map(|m| m.saturating_sub(used.requests)),
        max_tokens: budget.max_tokens.map(|m| m.saturating_sub(used.tokens)),
        max_cost_usd: budget.max_cost_usd.map(|m| (m - used.cost).max(0.0)),
    }
}

/// Which file becomes the checkpoint. A stopped agent may have been in the
/// middle of writing, so its last snapshot wins; an agent that exited by
/// itself is packaged as it left the work dir.
pub fn choose_checkpoint(ended: &str, final_ok: bool, have_snapshot: bool) -> &'static str {
    match (ended == "exited", final_ok, have_snapshot) {
        (true, true, _) => "final",
        (true, false, true) => "snapshot",
        (false, _, true) => "snapshot",
        (false, true, false) => "final",
        _ => "none",
    }
}

fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

pub(crate) fn read_usage(p: &Path) -> Vec<UsageRecord> {
    std::fs::read_to_string(p)
        .map(|s| crucible_report::usage::parse_jsonl(&s))
        .unwrap_or_default()
}

pub(super) fn current_user() -> Result<String> {
    let id = |flag: &str| -> Result<String> {
        let o = std::process::Command::new("id").arg(flag).output()?;
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    Ok(format!("{}:{}", id("-u")?, id("-g")?))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn package_to(stage: &Stage, work: PathBuf, cmd: Vec<String>, dest: PathBuf) -> Result<u64> {
    let (packager, opts) = (stage.packager.clone(), stage.packager_options.clone());
    tokio::task::spawn_blocking(move || -> Result<u64> {
        let (zip, _stats) = crate::packagers::package(&packager, &work, &cmd, opts.as_ref())?;
        write_atomic(&dest, &zip)?;
        Ok(zip.len() as u64)
    })
    .await?
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1e6)
}

pub(super) struct Env<'a, E: Executor> {
    pub exec: &'a E,
    pub args: &'a RunArgs,
    pub cred: &'a Credential,
    pub agent: &'a AgentSpec,
    pub pricing: &'a Pricing,
    pub user_price: Option<Price>,
    pub user: String,
    pub work: PathBuf,
    pub home: PathBuf,
    pub snaps: PathBuf,
}

enum Interrupt {
    Signal,
}

async fn shutdown_signal() -> Interrupt {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    Interrupt::Signal
}

/// Run one stage; returns its timing and whether the run was interrupted.
pub(super) async fn run_stage<E: Executor>(
    env: &Env<'_, E>,
    stage: &Stage,
    limits: Limits,
) -> Result<(Timing, bool)> {
    let a = env.args;
    let sdir = a.out_dir.join(&stage.id);
    std::fs::create_dir_all(&sdir)?;
    let usage_log = sdir.join("usage.jsonl");
    let egress_log = sdir.join("egress.jsonl");
    std::fs::File::create(&usage_log)?;
    std::fs::File::create(&egress_log)?;

    let meter_cfg = MeterConfig {
        upstream: Upstream::new(env.cred.clone(), false)?,
        model: a.model.clone(),
        pricing: env.pricing.clone(),
        user_price: env.user_price,
        force_usage: env.agent.streaming,
        limits,
        log_path: usage_log.clone(),
        insecure_allow_loopback_for_tests: false,
    };
    let meter_listener = TcpListener::bind((a.bind.as_str(), a.meter_port))
        .await
        .with_context(|| format!("meter: binding {}:{}", a.bind, a.meter_port))?;
    let egress_raw = std::fs::read_to_string(&a.egress_allow)?;
    let egress_cfg = crucible_egress::EgressConfig::from_json(&egress_raw, egress_log.clone())?;
    let egress_listener = TcpListener::bind((a.bind.as_str(), a.egress_port))
        .await
        .with_context(|| format!("egress: binding {}:{}", a.bind, a.egress_port))?;
    let meter = tokio::spawn(crucible_meter::serve(meter_listener, meter_cfg));
    let egress = tokio::spawn(crucible_egress::serve(egress_listener, egress_cfg));

    let req = std::fs::canonicalize(a.inputs_dir.join(&stage.id))
        .with_context(|| format!("inputs of stage {} are missing", stage.id))?;
    let spec = ContainerSpec {
        name: format!("crucible-{}-{}", stage.id, std::process::id()),
        image: a.image.clone(),
        network: a.network.clone(),
        user: env.user.clone(),
        req,
        work: env.work.clone(),
        home: env.home.clone(),
        agent_host: a.agent_host.clone().unwrap_or_else(|| a.bind.clone()),
        meter_port: a.meter_port,
        egress_port: a.egress_port,
        model: a.model.clone(),
        deadline_s: stage.time_limit_s,
        entrypoint: env.agent.entrypoint.clone(),
        label: if a.run_label.is_empty() {
            format!("pid-{}", std::process::id())
        } else {
            a.run_label.clone()
        },
    };
    let snapshot = env.snaps.join(format!("{}.zip", stage.id));
    let _ = std::fs::remove_file(&snapshot);
    let cmd = env.agent.app_start_cmd();

    eprintln!("[{}] start, limit {}s", stage.id, stage.time_limit_s);
    let started_at = now_rfc3339();
    let t0 = Instant::now();
    let exec = env.exec;
    let cid = exec.start(&agent_container(&spec)).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(stage.time_limit_s);
    let interval = Duration::from_secs(a.snapshot_interval_s.max(1));
    let mut next_snap = tokio::time::Instant::now() + interval;
    let mut wait = Box::pin(exec.wait(&cid));
    let mut snapshots = 0u32;
    let mut interrupted = false;
    let mut ended = "exited";
    let mut exit_code: Option<i64> = None;
    let signal = shutdown_signal();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            r = &mut wait => {
                exit_code = r.ok().flatten();
                break;
            }
            _ = tokio::time::sleep_until(next_snap) => {
                next_snap += interval;
                match package_to(stage, env.work.clone(), cmd.clone(), snapshot.clone()).await {
                    Ok(n) => { snapshots += 1; eprintln!("[{}] snapshot {snapshots} at {}s: {}", stage.id, t0.elapsed().as_secs(), mb(n)); }
                    Err(_) => eprintln!("[{}] snapshot at {}s: nothing to package yet", stage.id, t0.elapsed().as_secs()),
                }
            }
            _ = tokio::time::sleep_until(deadline) => { ended = "deadline"; break; }
            _ = &mut signal => { ended = "aborted"; interrupted = true; break; }
        }
    }
    if ended != "exited" {
        eprintln!("[{}] {ended}: stopping the container", stage.id);
        // Cancelled runs get little time: the runner kills this process soon.
        let grace = if interrupted { 2 } else { a.grace_s };
        exec.stop(&cid, Duration::from_secs(grace)).await;
        exit_code = wait.await.ok().flatten();
    }
    let wall_s = t0.elapsed().as_secs_f64();
    let ended_at = now_rfc3339();

    // Container output goes to agent.log only.
    let _ = exec
        .logs(&cid, LOG_TAIL_LINES, &sdir.join("agent.log"))
        .await;
    exec.remove(&cid).await;
    meter.abort();
    egress.abort();
    let _ = meter.await;
    let _ = egress.await;

    let ckpt = sdir.join("checkpoint.zip");
    let final_tmp = env.snaps.join(format!("{}.final.zip", stage.id));
    let final_ok = package_to(stage, env.work.clone(), cmd.clone(), final_tmp.clone())
        .await
        .is_ok();
    let source = choose_checkpoint(ended, final_ok, snapshot.is_file());
    match source {
        "final" => std::fs::rename(&final_tmp, &ckpt)?,
        "snapshot" => std::fs::copy(&snapshot, &ckpt).map(|_| ())?,
        _ => {}
    }
    let _ = std::fs::remove_file(&final_tmp);
    let checkpoint_bytes = std::fs::metadata(&ckpt).ok().map(|m| m.len());

    let timing = Timing {
        wall_s: (wall_s * 10.0).round() / 10.0,
        started_at,
        ended_at,
        time_limit_s: stage.time_limit_s,
        ended: ended.into(),
        exit_code,
        checkpoint_source: source.into(),
        checkpoint_bytes,
        snapshots,
    };
    write_atomic(
        &sdir.join("timing.json"),
        &serde_json::to_vec_pretty(&timing)?,
    )?;

    let records = read_usage(&usage_log);
    let u = used(&records);
    let missing = records.iter().filter(|r| r.usage_missing).count();
    let (mut allowed, mut denied) = (0, 0);
    for line in std::fs::read_to_string(&egress_log)
        .unwrap_or_default()
        .lines()
    {
        match serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v["allowed"].as_bool())
        {
            Some(true) => allowed += 1,
            Some(false) => denied += 1,
            None => {}
        }
    }
    eprintln!(
        "[{}] ended={ended} exit={} wall={:.0}s checkpoint={source} ({}) snapshots={snapshots} requests={} usage_missing={missing} egress_allowed={allowed} egress_denied={denied}",
        stage.id,
        exit_code.map_or("?".into(), |c| c.to_string()),
        wall_s,
        checkpoint_bytes.map_or("-".into(), mb),
        u.requests,
    );
    Ok((timing, interrupted))
}

/// agent.log keeps at most this many of the container's last output lines.
const LOG_TAIL_LINES: usize = 200_000;

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "crucible-stage-1-1".into(),
            image: "crucible-agent:run".into(),
            network: "crucible-sbx".into(),
            label: "t".into(),
            user: "1001:118".into(),
            req: "/r/inputs/stage-1".into(),
            work: "/r/work".into(),
            home: "/r/home".into(),
            agent_host: "172.31.250.1".into(),
            meter_port: 8787,
            egress_port: 3128,
            model: "glm-5.3-flash".into(),
            deadline_s: 4800,
            entrypoint: Some(vec!["/opt/agent/run.sh".into(), "--x".into()]),
        }
    }

    fn pair(a: &[String], flag: &str) -> Vec<String> {
        a.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    #[test]
    fn container_hardening() {
        let a = docker_run_args(&spec());
        assert_eq!(pair(&a, "--memory"), ["2g"]);
        assert_eq!(pair(&a, "--memory-swap"), ["2g"]);
        assert_eq!(pair(&a, "--cpus"), ["1"]);
        assert_eq!(pair(&a, "--pids-limit"), ["1024"]);
        assert_eq!(pair(&a, "--cap-drop"), ["ALL"]);
        assert_eq!(pair(&a, "--security-opt"), ["no-new-privileges"]);
        assert_eq!(pair(&a, "--user"), ["1001:118"]);
        assert_eq!(pair(&a, "--network"), ["crucible-sbx"]);
        assert_eq!(pair(&a, "--dns"), ["127.0.0.1"]);
        assert!(
            !a.iter()
                .any(|x| x == "--privileged" || x.contains("docker.sock"))
        );
        let mounts = pair(&a, "--mount");
        assert_eq!(
            mounts[0],
            "type=bind,source=/r/inputs/stage-1,target=/req,readonly"
        );
        assert_eq!(mounts[1], "type=bind,source=/r/work,target=/work");
        assert_eq!(mounts[2], "type=bind,source=/r/home,target=/home/agent");
        let env = pair(&a, "--env");
        for want in [
            "OPENAI_BASE_URL=http://172.31.250.1:8787/v1",
            "OPENAI_API_KEY=dummy",
            "MODEL=glm-5.3-flash",
            "DEADLINE_S=4800",
            "REQ_DIR=/req",
            "WORK_DIR=/work",
            "HOME=/home/agent",
            "HTTPS_PROXY=http://172.31.250.1:3128",
            "NO_PROXY=172.31.250.1,localhost,127.0.0.1",
        ] {
            assert!(env.iter().any(|e| e == want), "{want}");
        }
        // Entrypoint: first word via --entrypoint, the rest after the image.
        assert_eq!(pair(&a, "--entrypoint"), ["/opt/agent/run.sh"]);
        assert_eq!(&a[a.len() - 3..], ["--", "crucible-agent:run", "--x"]);
    }

    #[test]
    fn budget_carries_over_stages() {
        let b = Budget {
            max_requests: Some(10),
            max_tokens: None,
            max_cost_usd: Some(1.0),
            price: None,
        };
        let recs = vec![
            UsageRecord {
                prompt_tokens: Some(100),
                completion_tokens: Some(10),
                cost_usd: Some(0.4),
                ..Default::default()
            },
            UsageRecord {
                model_rejected: true,
                ..Default::default()
            },
            UsageRecord {
                prompt_tokens: Some(5),
                completion_tokens: Some(5),
                cost_usd: Some(0.7),
                ..Default::default()
            },
        ];
        let u = used(&recs);
        assert_eq!(u.requests, 2);
        assert_eq!(u.tokens, 120);
        let l = remaining(&b, u);
        assert_eq!(l.max_requests, Some(8));
        assert_eq!(l.max_tokens, None);
        assert_eq!(l.max_cost_usd, Some(0.0));
        assert!(remaining(&Budget::default(), u).is_unlimited());
    }

    #[test]
    fn checkpoint_choice() {
        assert_eq!(choose_checkpoint("exited", true, true), "final");
        assert_eq!(choose_checkpoint("exited", false, true), "snapshot");
        assert_eq!(choose_checkpoint("exited", false, false), "none");
        assert_eq!(choose_checkpoint("deadline", true, true), "snapshot");
        assert_eq!(choose_checkpoint("deadline", true, false), "final");
        assert_eq!(choose_checkpoint("aborted", false, false), "none");
    }
}
