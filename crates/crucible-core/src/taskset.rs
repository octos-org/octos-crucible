//! `taskset.json`: the stages of a task set, its scorer and how stage
//! scores add up.
//!
//! Each stage has two sealed blobs in the store: `inputs_blob` (a zip of
//! what the agent gets in `/req`) and `tests_blob` (a zip of the hidden test
//! material, read only by the scoring job). The generation job downloads
//! only `inputs_blob`.

use serde::{Deserialize, Serialize};

use crate::blob::BlobRef;

/// Platform limit on the summed stage time of one run (all stages run back
/// to back on one machine, inside a 6 h GitHub job).
pub const MAX_TOTAL_TIME_S: u64 = 18_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSet {
    pub schema: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub scorer: ScorerRef,
    #[serde(default)]
    pub aggregate: Aggregate,
    /// Declared wall clock of a whole run; at least the sum of the stage
    /// limits and at most [`MAX_TOTAL_TIME_S`].
    pub total_time_limit_s: u64,
    pub stages: Vec<Stage>,
}

/// Which scorer container grades the checkpoints, e.g. `playwright`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScorerRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// `passed` and `total` summed over stages.
    #[default]
    Sum,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    pub id: String,
    /// Sealed zip unpacked read-only into `/req` for this stage.
    pub inputs_blob: BlobRef,
    /// Sealed zip of the hidden test material for this stage.
    pub tests_blob: BlobRef,
    pub output: OutputKind,
    pub time_limit_s: u64,
    /// Number of tests the scorer is expected to report. A mismatch is
    /// flagged rather than silently summed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_total: Option<u32>,
}

/// What the agent must leave in its working directory at the end of a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputKind {
    /// A runnable web app (frontend + backend) served for browser tests;
    /// packaged as a zip with a Dockerfile at its root.
    WebApp,
    /// Arbitrary files, scored by a script or unit tests.
    Files,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TaskSetError {
    #[error("taskset: schema must be 1")]
    Schema,
    #[error("taskset: name must match [a-z0-9][a-z0-9-]{{0,63}}")]
    Name,
    #[error("taskset: at least one stage is required")]
    NoStages,
    #[error("taskset: stage id {0:?} must match [a-z0-9][a-z0-9-]{{0,63}}")]
    StageId(String),
    #[error("taskset: duplicate stage id {0:?}")]
    DuplicateStage(String),
    #[error("taskset: stage {0:?} needs time_limit_s > 0")]
    TimeLimit(String),
    #[error("taskset: stage {0:?} has an invalid blob reference")]
    Blob(String),
    #[error("taskset: stage limits add up to {sum_s}s, more than total_time_limit_s {total_s}s")]
    StagesExceedTotal { sum_s: u64, total_s: u64 },
    #[error("taskset: total time {total_s}s exceeds the platform limit {max_s}s")]
    TooLong { total_s: u64, max_s: u64 },
}

impl TaskSet {
    /// Sum of the stage limits.
    pub fn stage_time_s(&self) -> u64 {
        self.stages.iter().map(|s| s.time_limit_s).sum()
    }

    /// Registration check against the platform's per-run limit.
    pub fn validate(&self, max_total_s: u64) -> Result<(), TaskSetError> {
        if self.schema != 1 {
            return Err(TaskSetError::Schema);
        }
        if !crate::is_slug(&self.name, 64) {
            return Err(TaskSetError::Name);
        }
        if self.stages.is_empty() {
            return Err(TaskSetError::NoStages);
        }
        let mut seen = std::collections::HashSet::new();
        for s in &self.stages {
            if !crate::is_slug(&s.id, 64) {
                return Err(TaskSetError::StageId(s.id.clone()));
            }
            if !seen.insert(s.id.as_str()) {
                return Err(TaskSetError::DuplicateStage(s.id.clone()));
            }
            if s.time_limit_s == 0 {
                return Err(TaskSetError::TimeLimit(s.id.clone()));
            }
            if !s.inputs_blob.is_valid() || !s.tests_blob.is_valid() {
                return Err(TaskSetError::Blob(s.id.clone()));
            }
        }
        let sum_s = self.stage_time_s();
        if sum_s > self.total_time_limit_s {
            return Err(TaskSetError::StagesExceedTotal {
                sum_s,
                total_s: self.total_time_limit_s,
            });
        }
        if self.total_time_limit_s > max_total_s {
            return Err(TaskSetError::TooLong {
                total_s: self.total_time_limit_s,
                max_s: max_total_s,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(c: char) -> String {
        format!(
            r#"{{"sha256":"{}","key_id":"1ffa702796eb5ee8"}}"#,
            c.to_string().repeat(64)
        )
    }

    fn github() -> String {
        format!(
            r#"{{
          "schema": 1,
          "name": "github-full",
          "scorer": {{"name": "playwright"}},
          "aggregate": "sum",
          "total_time_limit_s": 9600,
          "stages": [
            {{"id": "stage-1", "inputs_blob": {a}, "tests_blob": {b}, "output": "web-app", "time_limit_s": 4800, "expected_total": 30}},
            {{"id": "stage-2", "inputs_blob": {c}, "tests_blob": {d}, "output": "web-app", "time_limit_s": 4800, "expected_total": 29}}
          ]
        }}"#,
            a = blob('a'),
            b = blob('b'),
            c = blob('c'),
            d = blob('d')
        )
    }

    #[test]
    fn parse_and_validate() {
        let t: TaskSet = serde_json::from_str(&github()).unwrap();
        assert_eq!(t.stage_time_s(), 9600);
        assert_eq!(t.stages[0].output, OutputKind::WebApp);
        t.validate(MAX_TOTAL_TIME_S).unwrap();
        assert_eq!(
            t.validate(3600),
            Err(TaskSetError::TooLong {
                total_s: 9600,
                max_s: 3600
            })
        );
        let mut t2 = t.clone();
        t2.total_time_limit_s = 9000;
        assert!(matches!(
            t2.validate(MAX_TOTAL_TIME_S),
            Err(TaskSetError::StagesExceedTotal { .. })
        ));
        let mut t3 = t;
        t3.total_time_limit_s = MAX_TOTAL_TIME_S + 1;
        assert!(matches!(
            t3.validate(MAX_TOTAL_TIME_S),
            Err(TaskSetError::TooLong { .. })
        ));
    }

    #[test]
    fn rejects_bad_stages() {
        let base: TaskSet = serde_json::from_str(&github()).unwrap();
        let mut t = base.clone();
        t.stages[1].id = "stage-1".into();
        assert!(matches!(
            t.validate(1 << 20),
            Err(TaskSetError::DuplicateStage(_))
        ));
        let mut t = base.clone();
        t.stages[0].tests_blob.sha256 = "xyz".into();
        assert!(matches!(t.validate(1 << 20), Err(TaskSetError::Blob(_))));
        let mut t = base;
        t.stages[0].time_limit_s = 0;
        assert!(matches!(
            t.validate(1 << 20),
            Err(TaskSetError::TimeLimit(_))
        ));
        assert!(serde_json::from_str::<OutputKind>("\"web_app\"").is_err());
    }
}
