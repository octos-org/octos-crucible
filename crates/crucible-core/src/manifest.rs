//! The manifest of one evaluation, committed to the `data` branch. Summary
//! tables and web pages are generated from manifests only.

use serde::{Deserialize, Serialize};

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
    /// Scores and statistics are private unless the submitter opts in.
    #[serde(default)]
    pub public: bool,
    pub replicas: Vec<ReplicaEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    pub name: String,
    pub version: String,
    /// The uploaded package, when it is not a builtin agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<BlobRef>,
}

/// A stored file: content address plus the key that sealed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// Lower-case hex SHA-256 of the stored (sealed) bytes.
    pub sha256: String,
    pub key_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaEntry {
    pub replica: u32,
    pub stages: Vec<StageEntry>,
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
            },
            model: "glm-5.3".into(),
            public: false,
            replicas: vec![ReplicaEntry {
                replica: 1,
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
                }],
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), m);
    }
}
