//! The manifest of one evaluation, committed to the `data` branch. Summary
//! tables and web pages are generated from manifests only.

use serde::{Deserialize, Serialize};

pub use crate::blob::BlobRef;
use crate::score::ScoreStatus;

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
    pub replicas: Vec<ReplicaEntry>,
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
    /// Why the replica produced no usable result, if it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageScore {
    pub status: ScoreStatus,
    pub passed: u32,
    pub total: u32,
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
            replicas: vec![ReplicaEntry {
                replica: 1,
                job: None,
                failure: None,
                stages: vec![StageEntry {
                    stage: "stage-1".into(),
                    score: Some(StageScore {
                        status: ScoreStatus::Failed,
                        passed: 27,
                        total: 30,
                    }),
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
                }],
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
        assert!(json.contains("\"mode\":\"agent\""));
        // Older manifests used `public`.
        let old = json.replace("\"score_public\":false", "\"public\":true");
        assert!(serde_json::from_str::<Manifest>(&old).unwrap().score_public);
    }
}
