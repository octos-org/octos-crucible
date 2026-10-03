//! Several replicas of one evaluation, each with several stages:
//! per-stage and overall statistics across replicas.
//!
//! A replica counts as failed — listed, counted, but left out of the
//! statistics — when any stage is missing, was never scored, or was scored
//! with an infrastructure error (`system_error`/`rejected`). An agent that
//! simply fails tests is not a failed replica; that is its score.
//!
//! On-disk layout read by [`load_run_dir`] (written by the run step):
//!
//! ```text
//! <dir>/<replica>/<stage>/usage.jsonl   meter log of that stage
//! <dir>/<replica>/<stage>/result.json   scorer output (absent = not scored)
//! <dir>/<replica>/<stage>/timing.json   {"wall_s": 1234.5}, from job timestamps
//! ```

use std::path::Path;

use crucible_core::{ScoreResult, ScoreStatus, UsageRecord};
use crucible_metering::{Price, Pricing};
use serde::{Deserialize, Serialize};

use crate::thousands;
use crate::usage::{parse_jsonl, summarise};

pub struct StageInput {
    pub stage: String,
    pub records: Vec<UsageRecord>,
    pub score: Option<ScoreResult>,
    pub wall_s: Option<f64>,
}

pub struct ReplicaInput {
    pub replica: String,
    pub stages: Vec<StageInput>,
}

/// One replica × stage (or a replica's total over its stages).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub stage: String,
    pub status: Option<ScoreStatus>,
    pub passed: u32,
    pub total: u32,
    pub wall_s: Option<f64>,
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_hit_rate: Option<f64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaReport {
    pub replica: String,
    pub failed: bool,
    pub failure: Option<String>,
    pub stages: Vec<Row>,
    pub total: Row,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub n: usize,
    pub mean: f64,
    /// Sample standard deviation (n − 1); `None` for a single value.
    pub std: Option<f64>,
    pub min: f64,
    pub max: f64,
}

/// Statistics per metric. A metric is `None` when any counted replica
/// lacks it (e.g. unknown price): a mean over a subset would mislead.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricStats {
    pub passed: Option<Stats>,
    pub total: Option<Stats>,
    pub wall_s: Option<Stats>,
    pub requests: Option<Stats>,
    pub prompt_tokens: Option<Stats>,
    pub cached_tokens: Option<Stats>,
    pub completion_tokens: Option<Stats>,
    pub reasoning_tokens: Option<Stats>,
    pub cache_hit_rate: Option<Stats>,
    pub cost_usd: Option<Stats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageStats {
    pub stage: String,
    pub stats: MetricStats,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub n_replicas: usize,
    pub n_ok: usize,
    pub n_failed: usize,
    pub replicas: Vec<ReplicaReport>,
    pub stages: Vec<StageStats>,
    pub total: MetricStats,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{path}: {err}")]
    Io { path: String, err: std::io::Error },
    #[error("{path}: {err}")]
    Json {
        path: String,
        err: serde_json::Error,
    },
}

pub fn stats(values: &[f64]) -> Option<Stats> {
    if values.is_empty() {
        return None;
    }
    let n = values.len();
    let mean = values.iter().sum::<f64>() / n as f64;
    let std = (n > 1).then(|| {
        let ss: f64 = values.iter().map(|v| (v - mean).powi(2)).sum();
        (ss / (n - 1) as f64).sqrt()
    });
    Some(Stats {
        n,
        mean,
        std,
        min: values.iter().copied().fold(f64::INFINITY, f64::min),
        max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    })
}

fn stage_row(s: &StageInput, pricing: &Pricing, user_price: Option<Price>) -> Row {
    let u = summarise(&s.records, pricing, user_price).total;
    Row {
        stage: s.stage.clone(),
        status: s.score.as_ref().map(|r| r.status),
        passed: s.score.as_ref().map_or(0, |r| r.passed),
        total: s.score.as_ref().map_or(0, |r| r.total),
        wall_s: s.wall_s,
        requests: u.requests,
        prompt_tokens: u.prompt_tokens,
        cached_tokens: u.cached_tokens,
        completion_tokens: u.completion_tokens,
        reasoning_tokens: u.reasoning_tokens,
        cache_hit_rate: u.cache_hit_rate,
        cost_usd: u.cost_usd,
    }
}

/// Sum of a replica's stages (the `sum` aggregate).
fn total_row(rows: &[Row]) -> Row {
    let sum_opt = |f: fn(&Row) -> Option<f64>| rows.iter().map(f).sum::<Option<f64>>();
    let mut t = Row {
        stage: "total".into(),
        wall_s: sum_opt(|r| r.wall_s),
        cost_usd: sum_opt(|r| r.cost_usd),
        ..Default::default()
    };
    for r in rows {
        t.passed += r.passed;
        t.total += r.total;
        t.requests += r.requests;
        t.prompt_tokens += r.prompt_tokens;
        t.cached_tokens += r.cached_tokens;
        t.completion_tokens += r.completion_tokens;
        t.reasoning_tokens += r.reasoning_tokens;
    }
    t.cache_hit_rate =
        (t.prompt_tokens > 0).then(|| t.cached_tokens as f64 / t.prompt_tokens as f64);
    t
}

fn metric_stats(rows: &[&Row]) -> MetricStats {
    let all = |f: &dyn Fn(&Row) -> Option<f64>| -> Option<Stats> {
        let v: Option<Vec<f64>> = rows.iter().map(|r| f(r)).collect();
        stats(&v?)
    };
    MetricStats {
        passed: all(&|r| Some(r.passed as f64)),
        total: all(&|r| Some(r.total as f64)),
        wall_s: all(&|r| r.wall_s),
        requests: all(&|r| Some(r.requests as f64)),
        prompt_tokens: all(&|r| Some(r.prompt_tokens as f64)),
        cached_tokens: all(&|r| Some(r.cached_tokens as f64)),
        completion_tokens: all(&|r| Some(r.completion_tokens as f64)),
        reasoning_tokens: all(&|r| Some(r.reasoning_tokens as f64)),
        cache_hit_rate: all(&|r| r.cache_hit_rate),
        cost_usd: all(&|r| r.cost_usd),
    }
}

pub fn aggregate(
    replicas: &[ReplicaInput],
    pricing: &Pricing,
    user_price: Option<Price>,
) -> RunReport {
    // Stage order: first appearance across replicas.
    let mut stage_names: Vec<String> = Vec::new();
    for r in replicas {
        for s in &r.stages {
            if !stage_names.contains(&s.stage) {
                stage_names.push(s.stage.clone());
            }
        }
    }
    let reports: Vec<ReplicaReport> = replicas
        .iter()
        .map(|r| {
            let rows: Vec<Row> = r
                .stages
                .iter()
                .map(|s| stage_row(s, pricing, user_price))
                .collect();
            let failure = stage_names.iter().find_map(|name| {
                match r.stages.iter().find(|s| &s.stage == name) {
                    None => Some(format!("stage {name} missing")),
                    Some(s) => match &s.score {
                        None => Some(format!("stage {name} not scored")),
                        Some(sc) if !sc.status.is_scored() => Some(format!(
                            "stage {name}: {}",
                            serde_json::to_value(sc.status)
                                .unwrap()
                                .as_str()
                                .unwrap_or("?")
                        )),
                        Some(_) => None,
                    },
                }
            });
            ReplicaReport {
                replica: r.replica.clone(),
                failed: failure.is_some(),
                failure,
                total: total_row(&rows),
                stages: rows,
            }
        })
        .collect();
    let ok: Vec<&ReplicaReport> = reports.iter().filter(|r| !r.failed).collect();
    let stages = stage_names
        .iter()
        .map(|name| {
            let rows: Vec<&Row> = ok
                .iter()
                .filter_map(|r| r.stages.iter().find(|s| &s.stage == name))
                .collect();
            StageStats {
                stage: name.clone(),
                stats: metric_stats(&rows),
            }
        })
        .collect();
    let totals: Vec<&Row> = ok.iter().map(|r| &r.total).collect();
    RunReport {
        n_replicas: reports.len(),
        n_ok: ok.len(),
        n_failed: reports.len() - ok.len(),
        total: metric_stats(&totals),
        stages,
        replicas: reports,
    }
}

/// Sort names so `r2` comes before `r10`.
fn sorted_dirs(dir: &Path) -> Result<Vec<String>, RunError> {
    let io = |err| RunError::Io {
        path: dir.display().to_string(),
        err,
    };
    let mut names = Vec::new();
    for e in std::fs::read_dir(dir).map_err(io)? {
        let e = e.map_err(io)?;
        if e.file_type().map_err(io)?.is_dir()
            && let Some(n) = e.file_name().to_str()
            && !n.starts_with('.')
        {
            names.push(n.to_owned());
        }
    }
    names.sort_by(|a, b| (a.len(), a).cmp(&(b.len(), b)));
    Ok(names)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, RunError> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|err| RunError::Json {
                path: path.display().to_string(),
                err,
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(RunError::Io {
            path: path.display().to_string(),
            err,
        }),
    }
}

#[derive(Deserialize)]
struct Timing {
    wall_s: f64,
}

pub fn load_run_dir(dir: &Path) -> Result<Vec<ReplicaInput>, RunError> {
    let mut out = Vec::new();
    for replica in sorted_dirs(dir)? {
        let rdir = dir.join(&replica);
        let mut stages = Vec::new();
        for stage in sorted_dirs(&rdir)? {
            let sdir = rdir.join(&stage);
            let records = match std::fs::read_to_string(sdir.join("usage.jsonl")) {
                Ok(s) => parse_jsonl(&s),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(err) => {
                    return Err(RunError::Io {
                        path: sdir.join("usage.jsonl").display().to_string(),
                        err,
                    });
                }
            };
            stages.push(StageInput {
                stage,
                records,
                score: read_json(&sdir.join("result.json"))?,
                wall_s: read_json::<Timing>(&sdir.join("timing.json"))?.map(|t| t.wall_s),
            });
        }
        out.push(ReplicaInput { replica, stages });
    }
    Ok(out)
}

fn fmt_stats(s: Option<Stats>, f: impl Fn(f64) -> String) -> String {
    match s {
        None => "n/a".into(),
        Some(s) => {
            let std = s.std.map_or(String::new(), |d| format!(" ± {}", f(d)));
            format!("{}{std} ({}–{})", f(s.mean), f(s.min), f(s.max))
        }
    }
}

pub fn markdown(r: &RunReport) -> String {
    let int = |v: f64| thousands(v.round() as u64);
    let one = |v: f64| format!("{v:.1}");
    let pct = |v: f64| format!("{:.1}%", v * 100.0);
    let usd = |v: f64| format!("${v:.4}");
    let mut out = format!(
        "replicas: {} (ok {}, failed {})\n\n\
         | stage | passed | total | wall s | requests | prompt | cached | completion | cache hit | equiv. cost |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
        r.n_replicas, r.n_ok, r.n_failed
    );
    let rows = r
        .stages
        .iter()
        .map(|s| (s.stage.as_str(), &s.stats))
        .chain(std::iter::once(("**total**", &r.total)));
    for (name, m) in rows {
        out.push_str(&format!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            fmt_stats(m.passed, one),
            fmt_stats(m.total, one),
            fmt_stats(m.wall_s, one),
            fmt_stats(m.requests, int),
            fmt_stats(m.prompt_tokens, int),
            fmt_stats(m.cached_tokens, int),
            fmt_stats(m.completion_tokens, int),
            fmt_stats(m.cache_hit_rate, pct),
            fmt_stats(m.cost_usd, usd),
        ));
    }
    out.push_str("\nmean ± sample std (min–max) over successful replicas.\n");
    let failed: Vec<String> = r
        .replicas
        .iter()
        .filter(|x| x.failed)
        .map(|x| format!("- {}: {}", x.replica, x.failure.as_deref().unwrap_or("")))
        .collect();
    if !failed.is_empty() {
        out.push_str(&format!("\nfailed replicas:\n{}\n", failed.join("\n")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(status: ScoreStatus, passed: u32, total: u32) -> Option<ScoreResult> {
        Some(ScoreResult {
            submission_id: None,
            task_id: None,
            visibility: None,
            status,
            passed,
            total,
            detail: String::new(),
            tests: None,
        })
    }

    fn usage(prompt: u64, cached: u64, completion: u64) -> Vec<UsageRecord> {
        vec![UsageRecord {
            resp_model: Some("glm-5".into()),
            status: 200,
            prompt_tokens: Some(prompt),
            cached_tokens: Some(cached),
            completion_tokens: Some(completion),
            reasoning_tokens: Some(0),
            ..Default::default()
        }]
    }

    fn stage(name: &str, s: Option<ScoreResult>, wall: f64, prompt: u64) -> StageInput {
        StageInput {
            stage: name.into(),
            records: usage(prompt, prompt / 2, 1000),
            score: s,
            wall_s: Some(wall),
        }
    }

    fn pricing() -> Pricing {
        Pricing::from_json(include_str!("../../../config/pricing.json")).unwrap()
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn stats_basics() {
        assert!(stats(&[]).is_none());
        let s = stats(&[5.0]).unwrap();
        assert_eq!((s.n, s.mean, s.std, s.min, s.max), (1, 5.0, None, 5.0, 5.0));
        let s = stats(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap();
        approx(s.mean, 5.0);
        approx(s.std.unwrap(), (32.0f64 / 7.0).sqrt()); // sample, not population
        assert_eq!((s.min, s.max), (2.0, 9.0));
    }

    #[test]
    fn replicas_and_stages() {
        let replicas = vec![
            ReplicaInput {
                replica: "r1".into(),
                stages: vec![
                    stage(
                        "stage-1",
                        score(ScoreStatus::Failed, 27, 30),
                        100.0,
                        1_000_000,
                    ),
                    stage(
                        "stage-2",
                        score(ScoreStatus::Failed, 20, 29),
                        200.0,
                        2_000_000,
                    ),
                ],
            },
            ReplicaInput {
                replica: "r2".into(),
                stages: vec![
                    stage(
                        "stage-1",
                        score(ScoreStatus::Passed, 30, 30),
                        300.0,
                        3_000_000,
                    ),
                    stage(
                        "stage-2",
                        score(ScoreStatus::Failed, 10, 29),
                        400.0,
                        4_000_000,
                    ),
                ],
            },
            // Scorer infrastructure fault: excluded from stats, counted.
            ReplicaInput {
                replica: "r3".into(),
                stages: vec![
                    stage("stage-1", score(ScoreStatus::Passed, 30, 30), 1.0, 1),
                    stage("stage-2", score(ScoreStatus::SystemError, 0, 0), 1.0, 1),
                ],
            },
            // Run died before stage 2.
            ReplicaInput {
                replica: "r4".into(),
                stages: vec![stage("stage-1", score(ScoreStatus::Failed, 1, 30), 1.0, 1)],
            },
        ];
        let r = aggregate(&replicas, &pricing(), None);
        assert_eq!((r.n_replicas, r.n_ok, r.n_failed), (4, 2, 2));
        assert_eq!(
            r.replicas[2].failure.as_deref(),
            Some("stage stage-2: system_error")
        );
        assert_eq!(
            r.replicas[3].failure.as_deref(),
            Some("stage stage-2 missing")
        );

        let s1 = &r.stages[0].stats;
        assert_eq!(r.stages[0].stage, "stage-1");
        let p = s1.passed.unwrap();
        assert_eq!((p.n, p.mean, p.min, p.max), (2, 28.5, 27.0, 30.0));
        approx(p.std.unwrap(), (4.5f64).sqrt());
        approx(s1.wall_s.unwrap().mean, 200.0);
        approx(s1.cache_hit_rate.unwrap().mean, 0.5);

        let t = &r.total;
        // r1: 47/59, r2: 40/59
        approx(t.passed.unwrap().mean, 43.5);
        approx(t.total.unwrap().mean, 59.0);
        approx(t.wall_s.unwrap().min, 300.0);
        approx(t.wall_s.unwrap().max, 700.0);
        // glm-5 r1 total: prompt 3M (1.5M cached), completion 2000
        let r1_cost = (1_500_000.0 * 1.0 + 1_500_000.0 * 0.2 + 2000.0 * 3.2) / 1e6;
        approx(r.replicas[0].total.cost_usd.unwrap(), r1_cost);
        let md = markdown(&r);
        assert!(md.contains("replicas: 4 (ok 2, failed 2)"), "{md}");
        assert!(md.contains("- r4: stage stage-2 missing"), "{md}");
        assert!(md.contains("| stage-1 | 28.5 ± 2.1 (27.0–30.0)"), "{md}");
    }

    #[test]
    fn unknown_price_or_wall_gives_no_stat() {
        let mut a = stage("s", score(ScoreStatus::Passed, 1, 1), 1.0, 10);
        a.records[0].resp_model = Some("kimi-for-coding".into());
        let mut b = stage("s", score(ScoreStatus::Passed, 1, 1), 1.0, 10);
        b.wall_s = None;
        let r = aggregate(
            &[
                ReplicaInput {
                    replica: "1".into(),
                    stages: vec![a],
                },
                ReplicaInput {
                    replica: "2".into(),
                    stages: vec![b],
                },
            ],
            &pricing(),
            None,
        );
        assert!(r.total.cost_usd.is_none());
        assert!(r.total.wall_s.is_none());
        assert_eq!(r.total.passed.unwrap().n, 2);
    }

    #[test]
    fn loads_directory_layout() {
        let dir = tempfile::tempdir().unwrap();
        for (rep, passed) in [("r2", 20), ("r10", 25), ("r1", 30)] {
            let s = dir.path().join(rep).join("stage-1");
            std::fs::create_dir_all(&s).unwrap();
            let rec = serde_json::to_string(&usage(1000, 0, 10)[0]).unwrap();
            std::fs::write(s.join("usage.jsonl"), format!("{rec}\n")).unwrap();
            std::fs::write(
                s.join("result.json"),
                format!(r#"{{"status":"failed","passed":{passed},"total":30}}"#),
            )
            .unwrap();
            std::fs::write(s.join("timing.json"), r#"{"wall_s": 60}"#).unwrap();
        }
        let reps = load_run_dir(dir.path()).unwrap();
        let names: Vec<_> = reps.iter().map(|r| r.replica.as_str()).collect();
        assert_eq!(names, ["r1", "r2", "r10"]);
        assert_eq!(reps[0].stages[0].wall_s, Some(60.0));
        let r = aggregate(&reps, &pricing(), None);
        approx(r.total.passed.unwrap().mean, 25.0);
    }
}
