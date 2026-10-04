//! `result.json` v2: the output of every scorer (docs/plugins.md §7,
//! docs/scorer-contract.md §4), and the stage score kept in a manifest.
//!
//! A continuous `score` (may be negative), an optional `max`, `status:
//! scored | error`, an optional `passed` flag and optional `items`.
//! Counting tests is the special case where every item is 0/1.
//!
//! The old format (`status: passed|failed|system_error|rejected` with
//! integer `passed / total`) is still read, converted by one fixed table
//! ([`Legacy`]); it is never rewritten. The two formats' status names are
//! disjoint, so the status alone tells them apart.

use serde::{Deserialize, Serialize};

use crate::taskset::{Aggregate, Combine};

/// `schema` of a v2 result.
pub const RESULT_SCHEMA: u32 = 2;
/// Bound on |score| and |max|.
pub const MAX_ABS_SCORE: f64 = 1e12;
pub const MAX_ITEMS: usize = 100;
pub const MAX_ITEM_NAME: usize = 100;
pub const MAX_DETAIL: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreStatus {
    /// The score says something about the agent (also when it produced
    /// nothing usable: then the score is whatever the scorer gives, usually
    /// 0). Counted in the totals.
    Scored,
    /// The scoring infrastructure failed: not counted, may be retried.
    Error,
}

impl ScoreStatus {
    pub fn is_scored(self) -> bool {
        self == ScoreStatus::Scored
    }
}

/// Kind of an `error` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The scorer itself failed (retry).
    System,
    /// The request was not a real submission.
    Rejected,
}

/// One line of a result's breakdown. An item with only `passed` counts as
/// 1 / 0 out of 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreItem {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
}

impl ScoreItem {
    /// (score, max) of this item: its own, or 1/0 out of 1 from `passed`.
    fn value(&self) -> Option<(f64, Option<f64>)> {
        match (self.score, self.passed) {
            (Some(s), _) => Some((s, self.max)),
            (None, Some(p)) => Some((if p { 1.0 } else { 0.0 }, Some(1.0))),
            (None, None) => None,
        }
    }
}

/// A scorer's `result.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawResult")]
pub struct ScoreResult {
    pub schema: u32,
    pub status: ScoreStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
    #[serde(default)]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ScoreItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<String>,
}

impl ScoreResult {
    pub fn scored(score: f64, max: Option<f64>, detail: impl Into<String>) -> Self {
        ScoreResult {
            schema: RESULT_SCHEMA,
            status: ScoreStatus::Scored,
            error: None,
            score: Some(score),
            max,
            passed: None,
            detail: detail.into(),
            items: None,
            visibility: None,
            task_id: None,
            submission_id: None,
        }
    }

    pub fn error(kind: ErrorKind, detail: impl Into<String>) -> Self {
        ScoreResult {
            status: ScoreStatus::Error,
            error: Some(kind),
            score: None,
            ..ScoreResult::scored(0.0, None, detail)
        }
    }

    /// Fill `score` / `max` from `items` when the scorer gave only items
    /// (by the taskset's `aggregate.items`), check the bounds of
    /// docs/plugins.md §7, and turn a result that breaks them into a system
    /// error. An `error` result keeps no score.
    pub fn finish(mut self, agg: &Aggregate) -> Self {
        if self.status == ScoreStatus::Error {
            self.error.get_or_insert(ErrorKind::System);
            self.score = None;
            self.max = None;
            return self;
        }
        self.error = None;
        if self.score.is_none()
            && let Some((s, m)) = self.items.as_deref().and_then(|i| combine_items(i, agg))
        {
            self.score = Some(s);
            if self.max.is_none() {
                self.max = m;
            }
        }
        if let Err(e) = self.check() {
            return ScoreResult {
                visibility: self.visibility,
                task_id: self.task_id,
                submission_id: self.submission_id,
                ..ScoreResult::error(ErrorKind::System, e)
            };
        }
        self
    }

    fn check(&self) -> Result<(), String> {
        let ok = |x: f64| x.is_finite() && x.abs() <= MAX_ABS_SCORE;
        match self.score {
            None => return Err("scorer gave no score".into()),
            Some(s) if !ok(s) => return Err("score out of range".into()),
            _ => {}
        }
        if self.max.is_some_and(|m| !ok(m)) {
            return Err("max out of range".into());
        }
        if self.detail.chars().count() > MAX_DETAIL {
            return Err("detail too long".into());
        }
        if let Some(items) = &self.items {
            if items.len() > MAX_ITEMS {
                return Err(format!("more than {MAX_ITEMS} items"));
            }
            for i in items {
                if i.name.chars().count() > MAX_ITEM_NAME
                    || i.score.is_some_and(|x| !ok(x))
                    || i.max.is_some_and(|x| !ok(x))
                {
                    return Err("an item is out of range".into());
                }
            }
        }
        Ok(())
    }
}

/// Stage (score, max) from items by `aggregate.items`; `None` when no item
/// has a value. `max` only when every counted item has one.
pub fn combine_items(items: &[ScoreItem], agg: &Aggregate) -> Option<(f64, Option<f64>)> {
    let vals: Vec<(f64, f64, Option<f64>)> = items
        .iter()
        .filter_map(|i| {
            let (s, m) = i.value()?;
            let w = match agg.items {
                Combine::Weighted => agg.item_weights.get(&i.name).copied().unwrap_or(0.0),
                _ => 1.0,
            };
            Some((w, s, m))
        })
        .collect();
    if vals.is_empty() {
        return None;
    }
    let score: f64 = vals.iter().map(|(w, s, _)| w * s).sum();
    let max: Option<f64> = vals.iter().map(|(w, _, m)| m.map(|m| w * m)).sum();
    Some(match agg.items {
        Combine::Mean => {
            let n = vals.len() as f64;
            (score / n, max.map(|m| m / n))
        }
        _ => (score, max),
    })
}

/// A stage's score in a manifest: the result without its text fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawResult")]
pub struct StageScore {
    pub status: ScoreStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<ScoreItem>>,
}

impl From<&ScoreResult> for StageScore {
    fn from(r: &ScoreResult) -> Self {
        StageScore {
            status: r.status,
            error: r.error,
            score: r.score,
            max: r.max,
            passed: r.passed,
            items: r.items.clone(),
        }
    }
}

impl StageScore {
    /// (score, max) when scored.
    pub fn value(&self) -> Option<(f64, Option<f64>)> {
        if self.status.is_scored() {
            Some((self.score.unwrap_or(0.0), self.max))
        } else {
            None
        }
    }
}

impl TryFrom<RawResult> for StageScore {
    type Error = String;
    fn try_from(r: RawResult) -> Result<Self, String> {
        ScoreResult::try_from(r).map(|r| StageScore::from(&r))
    }
}

/// The old (schema 1) status names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Legacy {
    Passed,
    Failed,
    SystemError,
    Rejected,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AnyStatus {
    New(ScoreStatus),
    Old(Legacy),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PassedField {
    Flag(bool),
    Count(u32),
}

/// An old-format test case (`tests[]`).
#[derive(Deserialize)]
struct TestCase {
    title: String,
    ok: bool,
}

/// Either format as written; see [`ScoreResult`] and [`StageScore`].
#[derive(Deserialize)]
struct RawResult {
    #[serde(default)]
    schema: Option<u32>,
    status: AnyStatus,
    #[serde(default)]
    error: Option<ErrorKind>,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    passed: Option<PassedField>,
    #[serde(default)]
    total: Option<u32>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    items: Option<Vec<ScoreItem>>,
    #[serde(default)]
    tests: Option<Vec<TestCase>>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    submission_id: Option<String>,
}

impl TryFrom<RawResult> for ScoreResult {
    type Error = String;
    fn try_from(r: RawResult) -> Result<Self, String> {
        let mut out = ScoreResult {
            schema: RESULT_SCHEMA,
            status: ScoreStatus::Scored,
            error: None,
            score: None,
            max: None,
            passed: None,
            detail: r.detail.unwrap_or_default(),
            items: None,
            visibility: r.visibility,
            task_id: r.task_id,
            submission_id: r.submission_id,
        };
        match r.status {
            AnyStatus::New(status) => {
                if r.schema.is_some_and(|s| s != RESULT_SCHEMA) {
                    return Err(format!("result schema must be {RESULT_SCHEMA}"));
                }
                out.passed = match r.passed {
                    None => None,
                    Some(PassedField::Flag(b)) => Some(b),
                    Some(PassedField::Count(_)) => return Err("passed must be a boolean".into()),
                };
                out.status = status;
                out.error = r.error;
                out.score = r.score;
                out.max = r.max;
                out.items = r.items;
            }
            // docs/plugins.md §7.1.
            AnyStatus::Old(old) => {
                let count = match r.passed {
                    None => 0,
                    Some(PassedField::Count(n)) => n,
                    Some(PassedField::Flag(_)) => return Err("passed must be a count".into()),
                };
                match old {
                    Legacy::Passed | Legacy::Failed => {
                        out.score = Some(f64::from(count));
                        out.max = Some(f64::from(r.total.unwrap_or(0)));
                        out.passed = Some(old == Legacy::Passed);
                    }
                    Legacy::SystemError => {
                        out.status = ScoreStatus::Error;
                        out.error = Some(ErrorKind::System);
                    }
                    Legacy::Rejected => {
                        out.status = ScoreStatus::Error;
                        out.error = Some(ErrorKind::Rejected);
                    }
                }
                out.items = r.tests.map(|t| {
                    t.into_iter()
                        .map(|c| ScoreItem {
                            name: c.title,
                            score: None,
                            max: None,
                            passed: Some(c.ok),
                        })
                        .collect()
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_old_format() {
        let raw = r#"{
          "submission_id": "s1", "task_id": "github-stage-1-req-test", "visibility": "public",
          "status": "failed", "passed": 27, "total": 30, "detail": "3/30 tests failed",
          "tests": [{"title": "t", "ok": true, "error": null, "screenshot": null}]
        }"#;
        let r: ScoreResult = serde_json::from_str(raw).unwrap();
        assert_eq!(r.status, ScoreStatus::Scored);
        assert_eq!((r.score, r.max, r.passed), (Some(27.0), Some(30.0), Some(false)));
        assert_eq!(r.items.as_ref().unwrap()[0].passed, Some(true));
        let all: ScoreResult =
            serde_json::from_str(r#"{"status":"passed","passed":1,"total":1}"#).unwrap();
        assert_eq!((all.score, all.max, all.passed), (Some(1.0), Some(1.0), Some(true)));
        let err: ScoreResult =
            serde_json::from_str(r#"{"status":"system_error","passed":0,"total":0}"#).unwrap();
        assert_eq!(
            (err.status, err.error, err.score),
            (ScoreStatus::Error, Some(ErrorKind::System), None)
        );
        let rej: StageScore = serde_json::from_str(r#"{"status":"rejected"}"#).unwrap();
        assert_eq!(rej.error, Some(ErrorKind::Rejected));
        assert!(serde_json::from_str::<ScoreResult>(r#"{"status":"bogus"}"#).is_err());
    }

    #[test]
    fn v2_round_trip_and_items() {
        let raw = r#"{"schema":2,"status":"scored","score":4458.556,"max":null,
          "detail":"survey_complete","items":[{"name":"sum_best_scores","score":4820.1},
          {"name":"required_penalty","score":-361.5}],"visibility":"hidden"}"#;
        let r: ScoreResult = serde_json::from_str(raw).unwrap();
        assert_eq!((r.score, r.max), (Some(4458.556), None));
        let back: ScoreResult = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert!(serde_json::from_str::<ScoreResult>(r#"{"schema":3,"status":"scored"}"#).is_err());
        assert!(
            serde_json::from_str::<ScoreResult>(r#"{"schema":2,"status":"scored","passed":3}"#)
                .is_err()
        );

        // Only items: 0/1 cases add up to passed / total.
        let agg = Aggregate::default();
        let r: ScoreResult = serde_json::from_str(
            r#"{"schema":2,"status":"scored","items":[{"name":"a","passed":true},{"name":"b","passed":false},{"name":"c","passed":true}]}"#,
        )
        .unwrap();
        let r = r.finish(&agg);
        assert_eq!((r.status, r.score, r.max), (ScoreStatus::Scored, Some(2.0), Some(3.0)));
        // No score at all: an error.
        let r: ScoreResult = serde_json::from_str(r#"{"schema":2,"status":"scored"}"#).unwrap();
        assert_eq!(r.finish(&agg).status, ScoreStatus::Error);
        let mut big = ScoreResult::scored(2e12, None, "");
        assert_eq!(big.clone().finish(&agg).error, Some(ErrorKind::System));
        big.score = Some(-5.5);
        assert_eq!(big.finish(&agg).score, Some(-5.5));
    }
}
