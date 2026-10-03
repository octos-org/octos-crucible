//! `result.json`: the fixed output of every scorer. Same shape as the
//! prototype grader's result.json so existing consumers keep working.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    pub status: ScoreStatus,
    #[serde(default)]
    pub passed: u32,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub detail: String,
    /// Absent for hidden tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<Vec<TestCase>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreStatus {
    /// All tests passed.
    Passed,
    /// Scored, some tests failed (or the app never came up): the agent's fault.
    Failed,
    /// Scorer-side fault: retry, do not count.
    SystemError,
    /// The request itself was not a real submission.
    Rejected,
}

impl ScoreStatus {
    /// Whether the score says something about the agent (vs. an
    /// infrastructure problem).
    pub fn is_scored(self) -> bool {
        matches!(self, ScoreStatus::Passed | ScoreStatus::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCase {
    pub title: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub screenshot: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_prototype_result() {
        let raw = r#"{
          "submission_id": "s1", "task_id": "github-stage-1-req-test", "visibility": "public",
          "status": "failed", "passed": 27, "total": 30, "detail": "3/30 tests failed",
          "tests": [{"title": "t", "ok": true, "error": null, "screenshot": null}]
        }"#;
        let r: ScoreResult = serde_json::from_str(raw).unwrap();
        assert_eq!((r.status, r.passed, r.total), (ScoreStatus::Failed, 27, 30));
        assert!(r.status.is_scored());
        assert_eq!(r.tests.unwrap().len(), 1);
        let hidden: ScoreResult =
            serde_json::from_str(r#"{"status":"system_error","passed":0,"total":0}"#).unwrap();
        assert!(!hidden.status.is_scored());
        assert!(hidden.tests.is_none());
    }
}
