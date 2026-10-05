//! `crucible seal-outputs` (generation job: seal each stage's checkpoint and
//! logs to the platform key) and `crucible manifest` (publish job: build the
//! evaluation manifest from the sealed files, usage logs, timing files and
//! GitHub's job timestamps).

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use crucible_core::TaskSet;
use crucible_core::manifest::{
    AgentRef, BlobRef, JobTiming, Manifest, Mode, ReplicaEntry, RunRef, StageEntry, StageScore,
    UsageTotals,
};
use crucible_crypto::{PrivateKey, PublicKey};
use crucible_metering::{Price, Pricing};
use serde::Deserialize;

use crate::runners::workdir::Timing;
use crate::zipdir::{self, ZipStats};

pub const LOG_FILES: [&str; 4] = ["agent.log", "egress.jsonl", "usage.jsonl", "timing.json"];

/// Seal `<replica_dir>/<stage>/checkpoint.zip` and a zip of its logs into
/// `<out>/<stage>/{checkpoint,logs}.sealed`, and `extra` files (agent facts)
/// into `<out>/agent.sealed`. Returns (stage, file, sha256).
pub fn seal_outputs(
    replica_dir: &Path,
    key: &PublicKey,
    out: &Path,
    extra: &[PathBuf],
) -> Result<Vec<(String, String, String)>> {
    let mut done = Vec::new();
    std::fs::create_dir_all(out)?;
    if !extra.is_empty() {
        let mut entries = Vec::new();
        for p in extra.iter().filter(|p| p.is_file()) {
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow::anyhow!("bad file name {}", p.display()))?;
            entries.push(zipdir::Entry {
                name: name.to_owned(),
                source: p.clone(),
                executable: false,
            });
        }
        let zip = zipdir::write_zip(Cursor::new(Vec::new()), &entries, &[])?.into_inner();
        let sealed = crucible_crypto::seal(key, &zip)?;
        std::fs::write(out.join("agent.sealed"), &sealed)?;
        done.push((
            "-".into(),
            "agent".into(),
            crucible_store::sha256_hex(&sealed),
        ));
    }
    let mut stages: Vec<String> = std::fs::read_dir(replica_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| crucible_core::is_slug(n, 64))
        .collect();
    stages.sort();
    for stage in stages {
        let sdir = replica_dir.join(&stage);
        let odir = out.join(&stage);
        std::fs::create_dir_all(&odir)?;
        let ckpt = sdir.join("checkpoint.zip");
        if std::fs::symlink_metadata(&ckpt).is_ok_and(|m| m.is_file()) {
            let sealed = crucible_crypto::seal(key, &std::fs::read(&ckpt)?)?;
            std::fs::write(odir.join("checkpoint.sealed"), &sealed)?;
            done.push((
                stage.clone(),
                "checkpoint".into(),
                crucible_store::sha256_hex(&sealed),
            ));
        }
        let mut entries = Vec::new();
        let mut stats = ZipStats::default();
        for f in LOG_FILES {
            zipdir::collect(&sdir, f, &|_| false, &mut entries, &mut stats)?;
        }
        if !entries.is_empty() {
            let zip = zipdir::write_zip(Cursor::new(Vec::new()), &entries, &[])?.into_inner();
            let sealed = crucible_crypto::seal(key, &zip)?;
            std::fs::write(odir.join("logs.sealed"), &sealed)?;
            done.push((stage, "logs".into(), crucible_store::sha256_hex(&sealed)));
        }
    }
    Ok(done)
}

#[derive(Debug, Deserialize)]
struct Jobs {
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
struct Job {
    id: u64,
    name: String,
    started_at: Option<String>,
    completed_at: Option<String>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize)]
struct Step {
    name: String,
    started_at: Option<String>,
    completed_at: Option<String>,
}

fn secs_between(a: &str, b: &str) -> Option<f64> {
    let a = humantime::parse_rfc3339_weak(a.trim_end_matches('Z')).ok()?;
    let b = humantime::parse_rfc3339_weak(b.trim_end_matches('Z')).ok()?;
    Some(b.duration_since(a).ok()?.as_secs_f64())
}

/// Timestamps of job `generate r<replica>` and its step `run_step`.
fn job_timing(jobs: &Jobs, replica: u32, run_step: &str) -> Option<JobTiming> {
    let job = jobs
        .jobs
        .iter()
        .find(|j| j.name == format!("generate r{replica}"))?;
    let (s, c) = (job.started_at.clone()?, job.completed_at.clone()?);
    let step = job.steps.iter().find(|st| st.name == run_step);
    let ss = step.and_then(|st| st.started_at.clone());
    let sc = step.and_then(|st| st.completed_at.clone());
    Some(JobTiming {
        job_id: job.id,
        wall_s: secs_between(&s, &c)?,
        run_step_wall_s: match (&ss, &sc) {
            (Some(a), Some(b)) => secs_between(a, b),
            _ => None,
        },
        started_at: s,
        completed_at: c,
        run_step_started_at: ss,
        run_step_completed_at: sc,
    })
}

/// Files of a sealed zip, by name.
pub fn read_sealed_zip(path: &Path, keys: &[PrivateKey]) -> Result<Vec<(String, Vec<u8>)>> {
    let plain = crucible_crypto::open(keys, &std::fs::read(path)?)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut a = zip::ZipArchive::new(Cursor::new(plain))?;
    let mut out = Vec::new();
    for i in 0..a.len() {
        let mut f = a.by_index(i)?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut buf)?;
        out.push((f.name().to_owned(), buf));
    }
    Ok(out)
}

/// usage.jsonl and timing.json of a stage: plain files if present, else
/// from its sealed logs.
type Raw = Option<Vec<u8>>;

pub(crate) fn stage_numbers(sdir: &Path, keys: &[PrivateKey]) -> Result<(Raw, Raw)> {
    let plain = |n: &str| std::fs::read(sdir.join(n)).ok();
    let (mut usage, mut timing) = (plain("usage.jsonl"), plain("timing.json"));
    let logs = sdir.join("logs.sealed");
    if (usage.is_none() || timing.is_none()) && !keys.is_empty() && logs.is_file() {
        for (name, data) in read_sealed_zip(&logs, keys)? {
            match name.as_str() {
                "usage.jsonl" if usage.is_none() => usage = Some(data),
                "timing.json" if timing.is_none() => timing = Some(data),
                _ => {}
            }
        }
    }
    Ok((usage, timing))
}

fn blob(path: &Path) -> Result<Option<BlobRef>> {
    if !path.is_file() {
        return Ok(None);
    }
    let data = std::fs::read(path)?;
    Ok(Some(BlobRef {
        sha256: crucible_store::sha256_hex(&data),
        key_id: crucible_crypto::sealed_key_id(&data)?,
    }))
}

/// Model use of the scoring job: `<dir>/{interactive,scorer}.jsonl` from
/// `crucible score` (the meters of the slots that used a model).
fn eval_usage(
    dir: &Path,
    pricing: &Pricing,
    user_price: Option<Price>,
) -> Option<crucible_core::manifest::EvalUsage> {
    let slot = |name: &str| {
        let raw = std::fs::read_to_string(dir.join(format!("{name}.jsonl"))).ok()?;
        let records = crucible_report::usage::parse_jsonl(&raw);
        let t = crucible_report::usage::summarise(&records, pricing, user_price).total;
        Some(crucible_core::manifest::SlotUsage {
            usage: UsageTotals {
                requests: t.requests,
                prompt_tokens: t.prompt_tokens,
                cached_tokens: t.cached_tokens,
                completion_tokens: t.completion_tokens,
                reasoning_tokens: t.reasoning_tokens,
            },
            cost_usd: t.cost_usd,
        })
    };
    let u = crucible_core::manifest::EvalUsage {
        interactive: slot("interactive"),
        scorer: slot("scorer"),
    };
    (u.interactive.is_some() || u.scorer.is_some()).then_some(u)
}

/// `failure.json` of the generation step, from `<replica>/agent.sealed`:
/// (category, details).
fn generation_failure(rdir: &Path, keys: &[PrivateKey]) -> (Option<String>, Option<String>) {
    let p = rdir.join("agent.sealed");
    if keys.is_empty() || !p.is_file() {
        return (None, None);
    }
    let Ok(files) = read_sealed_zip(&p, keys) else {
        return (None, None);
    };
    let Some(v) = files
        .iter()
        .find(|(n, _)| n == "failure.json")
        .and_then(|(_, d)| serde_json::from_slice::<serde_json::Value>(d).ok())
    else {
        return (None, None);
    };
    let text = |k: &str, max: usize| {
        v[k].as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(max).collect::<String>())
    };
    (text("category", 200), text("detail", 8000))
}

/// The category the scoring job recorded for replica `r`
/// (`<scores>/<r>/failure.json` or `<scores>/failure.json`).
fn scoring_failure(scores: &Path, r: u32) -> Option<String> {
    [scores.join(r.to_string()), scores.to_path_buf()]
        .iter()
        .find_map(|d| std::fs::read(d.join("failure.json")).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v["category"]
                .as_str()
                .map(|s| s.chars().take(200).collect())
        })
}

/// Why a stage got no test result: the scorer's detail when it errored or
/// scored 0. Scorers run with `--visibility hidden`, whose detail is a
/// fixed text with nothing of the tests (docs/scorer-contract.md).
fn stage_reason(r: &crucible_core::ScoreResult) -> Option<String> {
    let zero = r.status.is_scored() && r.score == Some(0.0);
    if r.status.is_scored() && !zero {
        return None;
    }
    let d = r.detail.trim();
    let d = if d.is_empty() {
        match (r.status.is_scored(), r.error.as_ref()) {
            (true, _) => return None,
            (false, Some(crucible_core::score::ErrorKind::Rejected)) => "output rejected",
            (false, _) => "scoring error",
        }
    } else {
        d
    };
    Some(d.chars().take(300).collect())
}

pub struct ManifestInputs<'a> {
    pub eval_id: &'a str,
    pub created_at: String,
    pub taskset: &'a TaskSet,
    /// `agent`: the first `stages_run` stages ran. `app`: only stage
    /// `stages_run` (1-based) of an uploaded output was scored.
    pub mode: Mode,
    pub stages_run: usize,
    pub model: &'a str,
    pub agent: AgentRef,
    /// `<dir>/<replica>/<stage>/{usage.jsonl,timing.json,*.sealed}`.
    pub results: &'a Path,
    pub replicas: u32,
    pub jobs_json: Option<&'a str>,
    pub run_step: &'a str,
    pub run: Option<RunRef>,
    pub pricing: &'a Pricing,
    pub user_price: Option<Price>,
    pub owner: Option<crucible_core::manifest::Owner>,
    pub score_public: bool,
    /// To read numbers from sealed logs.
    pub keys: &'a [PrivateKey],
    /// `<dir>/<replica>/<stage>/score.json` from `crucible score`.
    pub scores: Option<&'a Path>,
}

pub fn build_manifest(m: &ManifestInputs) -> Result<Manifest> {
    let jobs: Option<Jobs> = match m.jobs_json {
        Some(raw) => Some(serde_json::from_str(raw).context("parsing the jobs JSON")?),
        None => None,
    };
    let mut replicas = Vec::new();
    for r in 1..=m.replicas {
        let rdir = m.results.join(r.to_string());
        let job = jobs.as_ref().and_then(|j| job_timing(j, r, m.run_step));
        let mut stages = Vec::new();
        // What the generation step recorded (sealed with the agent facts):
        // a category, and details for the submitter.
        let (mut failure, failure_detail) = generation_failure(&rdir, m.keys);
        let stages_iter: Vec<&crucible_core::taskset::Stage> = match m.mode {
            Mode::Agent => m.taskset.stages.iter().take(m.stages_run).collect(),
            Mode::App => m
                .taskset
                .stages
                .get(m.stages_run.wrapping_sub(1))
                .into_iter()
                .collect(),
        };
        if stages_iter.is_empty() {
            bail!("no stage selected");
        }
        for st in stages_iter {
            let sdir = rdir.join(&st.id);
            let raw_score = match m.scores {
                Some(dir) => crate::score::read_score(dir, r, &st.id)?,
                None => None,
            };
            let score = raw_score.as_ref().map(StageScore::from);
            let reason = raw_score.as_ref().and_then(stage_reason);
            let (usage_raw, timing_raw) = stage_numbers(&sdir, m.keys)?;
            let timing: Option<Timing> = timing_raw.and_then(|b| serde_json::from_slice(&b).ok());
            if m.mode == Mode::Agent && timing.is_none() && failure.is_none() {
                failure = Some(format!("stage {} did not run", st.id));
            }
            let records = usage_raw
                .map(|b| crucible_report::usage::parse_jsonl(&String::from_utf8_lossy(&b)))
                .unwrap_or_default();
            let t = crucible_report::usage::summarise(&records, m.pricing, m.user_price).total;
            let output = blob(&sdir.join("checkpoint.sealed"))?;
            if (timing.is_some() || m.mode == Mode::App) && output.is_none() && failure.is_none() {
                failure = Some(format!("stage {} left no checkpoint", st.id));
            }
            let eval_usage = match m.scores {
                Some(dir) => eval_usage(
                    &dir.join(r.to_string()).join(&st.id).join("eval_usage"),
                    m.pricing,
                    m.user_price,
                ),
                None => None,
            };
            stages.push(StageEntry {
                stage: st.id.clone(),
                score,
                wall_s: timing.as_ref().map(|t| t.wall_s),
                usage: UsageTotals {
                    requests: t.requests,
                    prompt_tokens: t.prompt_tokens,
                    cached_tokens: t.cached_tokens,
                    completion_tokens: t.completion_tokens,
                    reasoning_tokens: t.reasoning_tokens,
                },
                cost_usd: t.cost_usd,
                output,
                logs: blob(&sdir.join("logs.sealed"))?,
                ended: timing.as_ref().map(|t| t.ended.clone()),
                exit_code: timing.as_ref().and_then(|t| t.exit_code),
                checkpoint_source: timing.as_ref().map(|t| t.checkpoint_source.clone()),
                eval_usage,
                reason,
            });
        }
        // An output that was never scored: the scoring job's category, if
        // it recorded one.
        if failure.is_none()
            && let Some(dir) = m.scores
            && let Some(st) = stages
                .iter()
                .find(|s| s.output.is_some() && s.score.is_none())
        {
            failure = Some(scoring_failure(dir, r).unwrap_or_else(|| {
                format!("stage {} was not scored (the scoring job failed)", st.stage)
            }));
        }
        // The stage clock runs inside the run step: it cannot exceed it.
        if let Some(step_s) = job.as_ref().and_then(|j| j.run_step_wall_s) {
            let sum: f64 = stages.iter().filter_map(|s| s.wall_s).sum();
            if sum > step_s + 60.0 {
                bail!("replica {r}: stage times ({sum:.0}s) exceed the run step ({step_s:.0}s)");
            }
        }
        if job.is_none() && jobs.is_some() && failure.is_none() {
            failure = Some("no GitHub job record".into());
        }
        replicas.push(ReplicaEntry {
            replica: r,
            job,
            failure,
            failure_detail,
            stages,
        });
    }
    let mut manifest = Manifest {
        schema: crucible_core::manifest::MANIFEST_SCHEMA,
        eval_id: m.eval_id.into(),
        created_at: m.created_at.clone(),
        taskset: m.taskset.name.clone(),
        agent: m.agent.clone(),
        model: m.model.into(),
        mode: m.mode,
        owner: m.owner.clone(),
        score_public: m.score_public,
        stages_run: (m.mode == Mode::Agent).then_some(m.stages_run as u32),
        run: m.run.clone(),
        timing_source: None,
        replicas,
        scoring: Some(m.taskset.scoring()),
        total_score: None,
        download: None,
    };
    manifest.total_score = manifest.compute_total_score();
    Ok(manifest)
}

/// The submitter's download: every stage's output and logs of every
/// replica, as `r<replica>/<stage>/{output,logs}.zip` in one AES-256 zip
/// locked with the download password.
/// `None` when there is nothing to pack (every replica failed early).
pub fn download_zip(
    results: &Path,
    keys: &[PrivateKey],
    password: &str,
) -> Result<Option<Vec<u8>>> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for f in sealed_files(results)? {
        let rel = f.strip_prefix(results)?;
        let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
        let name = match parts.as_slice() {
            [r, stage, "checkpoint.sealed"] => format!("r{r}/{stage}/output.zip"),
            [r, stage, "logs.sealed"] => format!("r{r}/{stage}/logs.zip"),
            _ => continue,
        };
        let plain = crucible_crypto::open(keys, &std::fs::read(&f)?)
            .with_context(|| format!("opening {}", f.display()))?;
        files.push((name, plain));
    }
    if files.is_empty() {
        return Ok(None);
    }
    let zip = crucible_crypto::write_password_zip(
        Cursor::new(Vec::new()),
        files.iter().map(|(n, d)| (n.as_str(), d.as_slice())),
        password,
    )?;
    Ok(Some(zip.into_inner()))
}

/// All `*.sealed` files under `dir`, sorted.
pub fn sealed_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() && e.path().extension().is_some_and(|x| x == "sealed") {
                out.push(e.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_crypto::PrivateKey;

    #[test]
    fn seal_then_manifest() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run/1");
        let s1 = run.join("stage-1");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::write(s1.join("checkpoint.zip"), b"PK-app").unwrap();
        std::fs::write(s1.join("agent.log"), "secret agent output").unwrap();
        std::fs::write(
            s1.join("usage.jsonl"),
            r#"{"ts":"t","path":"/v1/chat/completions","req_model":"glm-5.3-flash","status":200,"prompt_tokens":100,"cached_tokens":40,"completion_tokens":10}"#.to_owned() + "\n",
        )
        .unwrap();
        let timing = Timing {
            wall_s: 100.0,
            started_at: "2026-10-03T10:00:00Z".into(),
            ended_at: "2026-10-03T10:01:40Z".into(),
            time_limit_s: 4800,
            ended: "exited".into(),
            exit_code: Some(0),
            checkpoint_source: "final".into(),
            checkpoint_bytes: Some(6),
            snapshots: 0,
        };
        std::fs::write(s1.join("timing.json"), serde_json::to_vec(&timing).unwrap()).unwrap();

        let sk = PrivateKey::generate();
        let results = d.path().join("results/1");
        let facts = d.path().join("facts.json");
        std::fs::write(&facts, r#"{"agent":{"name":"octos"}}"#).unwrap();
        let done = seal_outputs(&run, &sk.public(), &results, &[facts]).unwrap();
        assert_eq!(done.len(), 3);
        let agent =
            read_sealed_zip(&results.join("agent.sealed"), std::slice::from_ref(&sk)).unwrap();
        assert_eq!(agent[0].0, "facts.json");
        let sealed = std::fs::read(results.join("stage-1/logs.sealed")).unwrap();
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
        let logs = crucible_crypto::open(std::slice::from_ref(&sk), &sealed).unwrap();
        let mut a = zip::ZipArchive::new(Cursor::new(logs)).unwrap();
        assert!(a.by_name("agent.log").is_ok());

        let blob = |c: char| {
            format!(
                r#"{{"sha256":"{}","key_id":"k"}}"#,
                c.to_string().repeat(64)
            )
        };
        let ts: TaskSet = serde_json::from_str(&format!(
            r#"{{"schema":1,"name":"demo","scorer":{{"name":"playwright"}},"total_time_limit_s":9600,
            "stages":[{{"id":"stage-1","inputs_blob":{},"tests_blob":{},"output":"web-app","time_limit_s":4800}},
                      {{"id":"stage-2","inputs_blob":{},"tests_blob":{},"output":"web-app","time_limit_s":4800}}]}}"#,
            blob('a'), blob('b'), blob('c'), blob('d')
        ))
        .unwrap();
        let jobs = r#"{"total_count":2,"jobs":[
          {"id":7,"name":"generate r1","started_at":"2026-10-03T09:55:00Z","completed_at":"2026-10-03T10:05:00Z",
           "steps":[{"name":"Run agent stages","started_at":"2026-10-03T09:59:50Z","completed_at":"2026-10-03T10:02:00Z"}]},
          {"id":8,"name":"publish","started_at":null,"completed_at":null,"steps":[]}]}"#;
        let pricing = Pricing::from_json(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/pricing.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let inputs = ManifestInputs {
            eval_id: "dev-1-1",
            created_at: "2026-10-03T10:06:00Z".into(),
            taskset: &ts,
            mode: Mode::Agent,
            stages_run: 1,
            model: "glm-5.3-flash",
            agent: AgentRef {
                name: "octos".into(),
                version: "1".into(),
                package: None,
                commit: Some("abc".into()),
            },
            results: &d.path().join("results"),
            replicas: 1,
            jobs_json: Some(jobs),
            run_step: "Run agent stages",
            run: None,
            pricing: &pricing,
            user_price: None,
            owner: None,
            score_public: false,
            keys: std::slice::from_ref(&sk),
            scores: None,
        };
        let m = build_manifest(&inputs).unwrap();
        let r = &m.replicas[0];
        assert_eq!(r.failure, None);
        let job = r.job.as_ref().unwrap();
        assert_eq!(job.wall_s, 600.0);
        assert_eq!(job.run_step_wall_s, Some(130.0));
        let s = &r.stages[0];
        assert_eq!(s.wall_s, Some(100.0));
        assert_eq!(s.usage.requests, 1);
        assert_eq!(s.usage.cached_tokens, 40);
        assert!(s.cost_usd.unwrap() > 0.0);
        assert_eq!(s.output.as_ref().unwrap().key_id, sk.public().key_id());
        assert_eq!(s.output.as_ref().unwrap().sha256, done[1].2);
        assert!(s.score.is_none());
        assert!(m.total_score.is_none());
        assert_eq!(r.stages.len(), 1);

        // With scores: the stage score and the total.
        let scores = d.path().join("scores/1/stage-1");
        std::fs::create_dir_all(&scores).unwrap();
        std::fs::write(
            scores.join("score.json"),
            r#"{"status":"failed","passed":2,"total":3,"detail":"1/3 tests failed"}"#,
        )
        .unwrap();
        let scores_dir = d.path().join("scores");
        let scored = build_manifest(&ManifestInputs {
            scores: Some(&scores_dir),
            created_at: inputs.created_at.clone(),
            agent: inputs.agent.clone(),
            run: inputs.run.clone(),
            owner: inputs.owner.clone(),
            ..inputs
        })
        .unwrap();
        let sc = scored.replicas[0].stages[0].score.clone().unwrap();
        assert_eq!((sc.score, sc.max), (Some(2.0), Some(3.0)));
        let snap = scored.scoring.as_ref().unwrap();
        assert_eq!(snap.plugins[0].name, ts.scorer.name);
        assert_eq!(scored.total_score, Some(0.6667));

        // The download: outputs and logs under a password.
        let z = download_zip(&d.path().join("results"), std::slice::from_ref(&sk), "pw")
            .unwrap()
            .unwrap();
        assert!(
            download_zip(&d.path().join("scores"), std::slice::from_ref(&sk), "pw")
                .unwrap()
                .is_none()
        );
        let files = crucible_crypto::read_password_zip(&z, "pw").unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, ["r1/stage-1/output.zip", "r1/stage-1/logs.zip"]);
        assert_eq!(files[0].1, b"PK-app");
        assert!(crucible_crypto::read_password_zip(&z, "nope").is_err());
        assert_eq!(sealed_files(&d.path().join("results")).unwrap().len(), 3);
        // Without the key the numbers cannot be read.
        let no_key = ManifestInputs {
            keys: &[],
            ..inputs
        };
        assert!(
            build_manifest(&no_key).unwrap().replicas[0]
                .failure
                .is_some()
        );
        let inputs = ManifestInputs {
            keys: std::slice::from_ref(&sk),
            ..no_key
        };

        // A stage clock longer than the GitHub step is refused.
        let bad = jobs.replace("10:02:00Z", "10:00:10Z");
        let inputs = ManifestInputs {
            jobs_json: Some(&bad),
            ..inputs
        };
        assert!(build_manifest(&inputs).is_err());
    }

    /// Why replicas failed: the generation step's sealed category and
    /// details, a stage the scorer could not score, the scoring job's
    /// category.
    #[test]
    fn failure_reasons() {
        let d = tempfile::tempdir().unwrap();
        let sk = PrivateKey::generate();
        let keys = std::slice::from_ref(&sk);
        let results = d.path().join("results");
        // r1: the agent image did not build.
        let run1 = d.path().join("run/1");
        std::fs::create_dir_all(&run1).unwrap();
        let facts = d.path().join("facts.json");
        std::fs::write(&facts, r#"{"agent":{"name":"x"}}"#).unwrap();
        let fail = d.path().join("failure.json");
        std::fs::write(
            &fail,
            r#"{"category":"agent image build failed","detail":"ERROR: unknown instruction: FORM"}"#,
        )
        .unwrap();
        seal_outputs(&run1, &sk.public(), &results.join("1"), &[facts, fail]).unwrap();
        // r2 and r3 left an output.
        for r in ["2", "3"] {
            let s1 = d.path().join("run").join(r).join("stage-1");
            std::fs::create_dir_all(&s1).unwrap();
            std::fs::write(s1.join("checkpoint.zip"), b"PK").unwrap();
            seal_outputs(
                &d.path().join("run").join(r),
                &sk.public(),
                &results.join(r),
                &[],
            )
            .unwrap();
        }
        // r2: the app did not build (scored 0); r3: the scoring job failed.
        let scores = d.path().join("scores");
        std::fs::create_dir_all(scores.join("2/stage-1")).unwrap();
        std::fs::write(
            scores.join("2/stage-1/score.json"),
            r#"{"schema":2,"status":"scored","score":0,"max":1,"detail":"app build failed"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(scores.join("3")).unwrap();
        std::fs::write(
            scores.join("3/failure.json"),
            r#"{"category":"scorer image build failed"}"#,
        )
        .unwrap();
        let blob = r#"{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","key_id":"k"}"#;
        let ts: TaskSet = serde_json::from_str(&format!(
            r#"{{"schema":1,"name":"demo","scorer":{{"name":"playwright"}},"total_time_limit_s":600,
            "stages":[{{"id":"stage-1","inputs_blob":{blob},"tests_blob":{blob},"output":"web-app","time_limit_s":600}}]}}"#
        ))
        .unwrap();
        let pricing = Pricing::from_json(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/pricing.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let m = build_manifest(&ManifestInputs {
            eval_id: "dev-1-1",
            created_at: "t".into(),
            taskset: &ts,
            mode: Mode::App,
            stages_run: 1,
            model: "",
            agent: AgentRef {
                name: "x".into(),
                version: "1".into(),
                package: None,
                commit: None,
            },
            results: &results,
            replicas: 3,
            jobs_json: None,
            run_step: "Run agent stages",
            run: None,
            pricing: &pricing,
            user_price: None,
            owner: None,
            score_public: false,
            keys,
            scores: Some(&scores),
        })
        .unwrap();
        let r = &m.replicas;
        assert_eq!(r[0].failure.as_deref(), Some("agent image build failed"));
        assert_eq!(
            r[0].failure_detail.as_deref(),
            Some("ERROR: unknown instruction: FORM")
        );
        assert_eq!(r[1].failure, None);
        assert_eq!(r[1].stages[0].reason.as_deref(), Some("app build failed"));
        assert_eq!(r[2].failure.as_deref(), Some("scorer image build failed"));
        assert_eq!(r[2].failure_detail, None);
    }
}
