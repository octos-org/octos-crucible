//! The manifest of one evaluation, committed to the `data` branch. Summary
//! tables and web pages are generated from manifests only.

use serde::{Deserialize, Serialize};

pub use crate::blob::BlobRef;
use crate::score::ScoreStatus;
pub use crate::score::StageScore;
use crate::taskset::{Aggregate, Scoring, StagesAgg};

/// `schema` of manifests written now: stage scores in the v2 result
/// format, with a `scoring` snapshot. Schema 1 manifests are read as is.
pub const MANIFEST_SCHEMA: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub eval_id: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub taskset: String,
    pub agent: AgentRef,
    pub model: String,
    /// `agent` (the platform ran an agent) or `app` (an uploaded output
    /// was only scored).
    #[serde(default)]
    pub mode: Mode,
    /// Who submitted it, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
    /// Scores and statistics are private unless the submitter opts in;
    /// only then is the manifest published in clear.
    #[serde(default, alias = "public")]
    pub score_public: bool,
    /// Which stages were run (all of the taskset's unless a dev run asked
    /// for the first N).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stages_run: Option<u32>,
    /// GitHub Actions run that produced this evaluation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunRef>,
    /// Where the wall times come from when not from a GitHub run
    /// (`local`: the machine's own clock, `crucible eval local`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_source: Option<String>,
    pub replicas: Vec<ReplicaEntry>,
    /// How this evaluation was scored and is shown, from its taskset at
    /// publish time. Absent in manifests that predate it: those read as
    /// `ratio` with test counts ([`crate::taskset::legacy_display`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<Scoring>,
    /// By the snapshot's aggregate ([`Manifest::compute_total_score`]);
    /// absent when nothing was scored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_score: Option<f64>,
    /// The submitter's download: an AES-256 zip (download password) of the
    /// stage outputs and logs, stored as a plain blob so the Worker can hand
    /// out its address (`GET /evals/:id/download`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download: Option<DownloadRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadRef {
    /// SHA-256 of the stored zip (its blob name).
    pub sha256: String,
}

impl Manifest {
    /// The total by the snapshot's aggregate (default `ratio`), rounded to
    /// 4 decimals. The one rule used by `crucible manifest`, the Worker and
    /// `crucible status`; see [`total_score`].
    pub fn compute_total_score(&self) -> Option<f64> {
        let default = Aggregate::default();
        let agg = self.scoring.as_ref().map_or(&default, |s| &s.aggregate);
        total_score(agg, &self.replicas)
    }
}

/// Total of a set of replicas (docs/plugins.md §8).
///
/// `ratio`: Σscore / Σmax over every scored stage of every replica; errors
/// and unscored stages add nothing; `None` when Σmax is 0. This is exactly
/// the old Σpassed / Σtotal, so old evaluations keep their totals.
///
/// Otherwise the total of each replica (`sum`, `mean` or `weighted` of its
/// scored stages; with `normalize`, of score / max), then the mean over
/// replicas. A replica with an `error` stage, or with no scored stage, is
/// left out; `None` when none is left.
pub fn total_score(agg: &Aggregate, replicas: &[ReplicaEntry]) -> Option<f64> {
    let round = |x: f64| (x * 10_000.0).round() / 10_000.0;
    if agg.stages == StagesAgg::Ratio {
        let (score, max) = replicas
            .iter()
            .flat_map(|r| r.stages.iter())
            .filter_map(|s| s.score.as_ref()?.value())
            .fold((0.0, 0.0), |(a, b), (s, m)| (a + s, b + m.unwrap_or(0.0)));
        return (max > 0.0).then(|| round(score / max));
    }
    let per: Vec<f64> = replicas
        .iter()
        .filter_map(|r| {
            let scores: Vec<&StageScore> =
                r.stages.iter().filter_map(|s| s.score.as_ref()).collect();
            if scores.iter().any(|s| s.status == ScoreStatus::Error) {
                return None;
            }
            let vals: Vec<(f64, f64)> = r
                .stages
                .iter()
                .filter_map(|s| {
                    let (v, m) = s.score.as_ref()?.value()?;
                    let v = if agg.normalize && agg.stages != StagesAgg::Sum {
                        v / m.filter(|m| *m > 0.0)?
                    } else {
                        v
                    };
                    let w = agg.weights.get(&s.stage).copied().unwrap_or(0.0);
                    Some((v, w))
                })
                .collect();
            if vals.is_empty() {
                return None;
            }
            Some(match agg.stages {
                StagesAgg::Mean => vals.iter().map(|(v, _)| v).sum::<f64>() / vals.len() as f64,
                StagesAgg::Weighted => vals.iter().map(|(v, w)| v * w).sum(),
                _ => vals.iter().map(|(v, _)| v).sum(),
            })
        })
        .collect();
    if per.is_empty() {
        return None;
    }
    Some(round(per.iter().sum::<f64>() / per.len() as f64))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Agent,
    App,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub github_id: u64,
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRef {
    pub repository: String,
    pub run_id: u64,
    pub run_attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub name: String,
    pub version: String,
    /// The uploaded package, when it is not a builtin agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<BlobRef>,
    /// Source revision recorded by the agent image build, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaEntry {
    pub replica: u32,
    /// Timestamps of the generation job, from the GitHub API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<JobTiming>,
    /// Why the replica produced no usable result, if it did not: a short
    /// category (e.g. "agent image build failed"), safe for public logs and
    /// the data branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Details for the submitter only (e.g. the tail of the agent's image
    /// build log). Never from a machine that held the tests; dropped from
    /// the data branch and never printed in public logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    pub stages: Vec<StageEntry>,
}

/// GitHub's own timestamps for a job and for its agent-run step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobTiming {
    pub job_id: u64,
    pub started_at: String,
    pub completed_at: String,
    pub wall_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_step_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_step_completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_step_wall_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageEntry {
    pub stage: String,
    /// `None` when the stage was never scored (run aborted before it).
    #[serde(default)]
    pub score: Option<StageScore>,
    /// Wall time from the job's own timestamps, not the agent's report.
    #[serde(default)]
    pub wall_s: Option<f64>,
    pub usage: UsageTotals,
    /// `None` when the model's price is unknown.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<BlobRef>,
    /// How the stage ended: `exited` (agent stopped by itself), `deadline`
    /// (stopped at the time limit) or `aborted` (the run was cancelled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// `final` (work dir at the end) or `snapshot` (last periodic snapshot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_source: Option<String>,
    /// Model use of the scoring job (interactive runner, judge scorer),
    /// kept apart from `usage` (the agent producing its output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_usage: Option<EvalUsage>,
    /// Why the stage has no test result (output rejected, app did not
    /// build, scoring error): the scorer's own message when its tests are
    /// public, else only the category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Model use while scoring, per slot (docs/plugins.md §10).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive: Option<SlotUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scorer: Option<SlotUsage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlotUsage {
    pub usage: UsageTotals,
    /// `None` when the model's price is unknown.
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let m = Manifest {
            schema: 1,
            eval_id: "ev_0001".into(),
            created_at: "2026-10-03T00:00:00Z".into(),
            taskset: "github-full".into(),
            agent: AgentRef {
                name: "octos".into(),
                version: "1".into(),
                package: None,
                commit: None,
            },
            model: "glm-5.3".into(),
            mode: Mode::Agent,
            owner: Some(Owner {
                github_id: 1,
                login: "octocat".into(),
            }),
            score_public: false,
            stages_run: None,
            run: None,
            timing_source: None,
            scoring: None,
            total_score: Some(0.9),
            download: None,
            replicas: vec![ReplicaEntry {
                replica: 1,
                job: None,
                failure: None,
                failure_detail: None,
                stages: vec![StageEntry {
                    stage: "stage-1".into(),
                    score: serde_json::from_str(r#"{"status":"failed","passed":27,"total":30}"#)
                        .unwrap(),
                    wall_s: Some(1800.0),
                    usage: UsageTotals::default(),
                    cost_usd: None,
                    output: Some(BlobRef {
                        sha256: "00".repeat(32),
                        key_id: "k".into(),
                    }),
                    logs: None,
                    ended: Some("exited".into()),
                    exit_code: Some(0),
                    checkpoint_source: Some("final".into()),
                    eval_usage: None,
                    reason: None,
                }],
            }],
        };
        assert_eq!(m.compute_total_score(), Some(0.9));
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
        assert!(json.contains("\"mode\":\"agent\""));
        // Older manifests used `public`.
        let old = json.replace("\"score_public\":false", "\"public\":true");
        assert!(serde_json::from_str::<Manifest>(&old).unwrap().score_public);
    }

    fn legacy(raw: &str) -> Manifest {
        serde_json::from_str(raw).unwrap()
    }

    /// Shapes of evaluations published before result v2 (7b444600…,
    /// 90fa75ee…, and an astro L1 run): same totals as before.
    #[test]
    fn old_manifests_keep_their_totals() {
        let head = r#""schema":1,"eval_id":"e","created_at":"t","taskset":"hello-world","agent":{"name":"a","version":"1"},"model":"m""#;
        let usage = r#""usage":{"requests":0,"prompt_tokens":0,"cached_tokens":0,"completion_tokens":0,"reasoning_tokens":0}"#;
        let app = legacy(&format!(
            r#"{{{head},"mode":"app","replicas":[{{"replica":1,"stages":[{{"stage":"stage-1","score":{{"status":"passed","passed":1,"total":1}},{usage}}}]}}],"total_score":1.0}}"#
        ));
        assert_eq!(app.compute_total_score(), Some(1.0));
        let s = app.replicas[0].stages[0].score.as_ref().unwrap();
        assert_eq!(
            (s.status, s.score, s.max, s.passed),
            (ScoreStatus::Scored, Some(1.0), Some(1.0), Some(true))
        );
        let agent = legacy(&format!(
            r#"{{{head},"replicas":[{{"replica":1,"stages":[{{"stage":"stage-1","score":{{"status":"failed","passed":0,"total":1}},{usage}}},{{"stage":"stage-2","score":{{"status":"failed","passed":0,"total":1}},{usage}}}]}}],"total_score":0.0}}"#
        ));
        assert_eq!(agent.compute_total_score(), Some(0.0));
        let astro = legacy(&format!(
            r#"{{{head},"mode":"app","replicas":[{{"replica":1,"stages":[{{"stage":"l1","score":{{"status":"failed","passed":4458556,"total":10000000}},{usage}}},{{"stage":"l2","score":{{"status":"system_error","passed":0,"total":0}},{usage}}},{{"stage":"l3","score":null,{usage}}}]}}]}}"#
        ));
        assert_eq!(astro.compute_total_score(), Some(0.4459));
        // Written back, it is the new format; read again, the same total.
        let again: Manifest =
            serde_json::from_str(&serde_json::to_string(&astro).unwrap()).unwrap();
        assert_eq!(again.compute_total_score(), Some(0.4459));
        assert!(
            serde_json::to_string(&again)
                .unwrap()
                .contains(r#""status":"error","error":"system""#)
        );
    }

    #[test]
    fn totals_by_aggregate() {
        use crate::taskset::{Display, PluginVersion};
        let st = |id: &str, score: Option<f64>, max: Option<f64>, error: bool| StageEntry {
            stage: id.into(),
            score: Some(StageScore {
                status: if error {
                    ScoreStatus::Error
                } else {
                    ScoreStatus::Scored
                },
                error: None,
                score,
                max,
                passed: None,
                items: None,
            }),
            wall_s: None,
            usage: UsageTotals::default(),
            cost_usd: None,
            output: None,
            logs: None,
            ended: None,
            exit_code: None,
            checkpoint_source: None,
            eval_usage: None,
            reason: None,
        };
        let rep = |n: u32, stages: Vec<StageEntry>| ReplicaEntry {
            replica: n,
            job: None,
            failure: None,
            failure_detail: None,
            stages,
        };
        let reps = vec![
            rep(
                1,
                vec![
                    st("l1", Some(4458.556), None, false),
                    st("l2", Some(-100.0), None, false),
                ],
            ),
            rep(
                2,
                vec![
                    st("l1", Some(1000.0), None, false),
                    st("l2", None, None, true),
                ],
            ),
        ];
        let agg = |j: &str| -> Aggregate { serde_json::from_str(j).unwrap() };
        // The error replica is left out.
        assert_eq!(
            total_score(&agg(r#"{"stages":"sum"}"#), &reps),
            Some(4358.556)
        );
        assert_eq!(
            total_score(&agg(r#"{"stages":"mean"}"#), &reps),
            Some(2179.278)
        );
        assert_eq!(
            total_score(&agg(r#"{"stages":"weighted","weights":{"l1":2}}"#), &reps),
            Some(8917.112)
        );
        assert_eq!(total_score(&agg(r#"{"stages":"ratio"}"#), &reps), None);
        let counted = vec![
            rep(
                1,
                vec![
                    st("a", Some(1.0), Some(2.0), false),
                    st("b", Some(3.0), Some(4.0), false),
                ],
            ),
            rep(
                2,
                vec![
                    st("a", Some(2.0), Some(2.0), false),
                    st("b", Some(0.0), Some(4.0), false),
                ],
            ),
        ];
        assert_eq!(total_score(&agg(r#""sum""#), &counted), Some(0.5));
        assert_eq!(
            total_score(&agg(r#"{"stages":"mean","normalize":true}"#), &counted),
            Some(0.5625)
        );
        let m = Manifest {
            scoring: Some(Scoring {
                aggregate: agg(r#"{"stages":"sum"}"#),
                display: Display::default(),
                plugins: vec![PluginVersion {
                    kind: "scorer".into(),
                    name: "x".into(),
                    version: "1".into(),
                }],
            }),
            replicas: reps,
            ..legacy(
                r#"{"schema":2,"eval_id":"e","created_at":"t","taskset":"t","agent":{"name":"a","version":"1"},"model":"m","replicas":[]}"#,
            )
        };
        assert_eq!(m.compute_total_score(), Some(4358.556));
    }
}
