//! `taskset.json`: the stages of a task set, its scorer and how stage
//! scores add up.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSet {
    pub schema: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub scorer: ScorerRef,
    #[serde(default)]
    pub aggregate: Aggregate,
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
    /// Paths inside the taskset package handed to the agent for this stage
    /// (read-only); typically the stage's requirement document.
    pub inputs: Vec<String>,
    pub output: OutputKind,
    pub time_limit_s: u64,
    /// Number of tests the scorer is expected to report. A mismatch is
    /// flagged rather than silently summed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_total: Option<u32>,
}

/// What the agent must leave in its working directory at the end of a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    /// A runnable web app (frontend + backend) served for browser tests.
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
    #[error("taskset: stage {0:?} input {1:?} must be a relative path without '..'")]
    Input(String, String),
    #[error("taskset: total time {total_s}s exceeds the platform limit {max_s}s")]
    TooLong { total_s: u64, max_s: u64 },
}

impl TaskSet {
    pub fn total_time_s(&self) -> u64 {
        self.stages.iter().map(|s| s.time_limit_s).sum()
    }

    /// Registration check. `max_total_s` is the platform's per-run wall
    /// clock limit (all stages run back to back on one machine).
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
            for i in &s.inputs {
                let bad = i.is_empty()
                    || i.starts_with('/')
                    || i.contains('\\')
                    || i.split('/').any(|c| c == "..");
                if bad {
                    return Err(TaskSetError::Input(s.id.clone(), i.clone()));
                }
            }
        }
        let total_s = self.total_time_s();
        if total_s > max_total_s {
            return Err(TaskSetError::TooLong {
                total_s,
                max_s: max_total_s,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = r#"{
      "schema": 1,
      "name": "github-full",
      "scorer": {"name": "playwright"},
      "aggregate": "sum",
      "stages": [
        {"id": "stage-1", "inputs": ["stage-1/requirements.md"], "output": "web_app", "time_limit_s": 3600, "expected_total": 30},
        {"id": "stage-2", "inputs": ["stage-2/requirements.md"], "output": "web_app", "time_limit_s": 3600, "expected_total": 29}
      ]
    }"#;

    #[test]
    fn parse_and_validate() {
        let t: TaskSet = serde_json::from_str(GITHUB).unwrap();
        assert_eq!(t.total_time_s(), 7200);
        t.validate(6 * 3600).unwrap();
        assert_eq!(
            t.validate(3600),
            Err(TaskSetError::TooLong {
                total_s: 7200,
                max_s: 3600
            })
        );
    }

    #[test]
    fn rejects_bad_stages() {
        let mut t: TaskSet = serde_json::from_str(GITHUB).unwrap();
        t.stages[1].id = "stage-1".into();
        assert!(matches!(
            t.validate(1 << 20),
            Err(TaskSetError::DuplicateStage(_))
        ));
        let mut t: TaskSet = serde_json::from_str(GITHUB).unwrap();
        t.stages[0].inputs = vec!["../secret".into()];
        assert!(matches!(t.validate(1 << 20), Err(TaskSetError::Input(..))));
        let mut t: TaskSet = serde_json::from_str(GITHUB).unwrap();
        t.stages[0].time_limit_s = 0;
        assert!(matches!(
            t.validate(1 << 20),
            Err(TaskSetError::TimeLimit(_))
        ));
    }
}
