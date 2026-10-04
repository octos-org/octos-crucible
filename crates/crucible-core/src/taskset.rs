//! `taskset.json`: the stages of a task set, its scorer and how stage
//! scores add up.
//!
//! Each stage has two sealed blobs in the store: `inputs_blob` (a zip of
//! what the agent gets in `/req`) and `tests_blob` (a zip of the hidden test
//! material, read only by the scoring job). The generation job downloads
//! only `inputs_blob`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::blob::BlobRef;

/// Platform limit on the summed stage time of one run (all stages run back
/// to back on one machine, inside a 6 h GitHub job).
pub const MAX_TOTAL_TIME_S: u64 = 18_000;

/// Scorers a user-uploaded taskset may name: those reviewed for untrusted
/// test material (docs/scorer-contract.md §7).
pub const USER_SCORERS: &[&str] = &["playwright"];

/// `u-` + 16 lower-case hex: the id of a user-uploaded taskset. Built-in
/// taskset directories never start with `u-`.
pub fn is_user_taskset_id(s: &str) -> bool {
    s.strip_prefix("u-").is_some_and(|h| {
        h.len() == 16
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSet {
    /// 1, or 2 for tasksets that use the object form of `aggregate`,
    /// `display` or `model` (docs/plugins.md §8–§10).
    pub schema: u32,
    pub name: String,
    /// Display name. User-uploaded tasksets are registered under a
    /// platform id (`u-...`) as `name`; the name from their source.json
    /// is kept here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub description: String,
    pub scorer: ScorerRef,
    #[serde(default)]
    pub aggregate: Aggregate,
    /// How scores are shown; see [`Display`].
    #[serde(default, skip_serializing_if = "Display::is_empty")]
    pub display: Display,
    /// Model use while scoring (docs/plugins.md §10). Parsed, not used yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelDecl>,
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

/// Behaviour versions of the scorers (bumped when scoring changes);
/// recorded in each manifest's `scoring` snapshot. Replaced by the plugin
/// registry in P2 (docs/plugins.md §3).
pub const SCORER_VERSIONS: &[(&str, &str)] = &[("playwright", "1"), ("astro-survey", "2")];

/// How the scores of items (within a stage) and of stages add up
/// (docs/plugins.md §8). Schema 1's `"aggregate": "sum"` (Σpassed /
/// Σtotal) reads as `{"stages": "ratio"}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawAggregate")]
pub struct Aggregate {
    /// Items → stage score, when a scorer gave only items.
    #[serde(default)]
    pub items: Combine,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub item_weights: BTreeMap<String, f64>,
    /// Stages → total.
    #[serde(default)]
    pub stages: StagesAgg,
    /// By stage id; unlisted stages weigh 0.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub weights: BTreeMap<String, f64>,
    /// For `mean` / `weighted` stages: use score / max of each stage.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub normalize: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Combine {
    #[default]
    Sum,
    Mean,
    Weighted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagesAgg {
    /// Σscore / Σmax over every scored stage of every replica (0–1).
    #[default]
    Ratio,
    Sum,
    Mean,
    Weighted,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawAggregate {
    Legacy(LegacyAggregate),
    Object(AggregateObject),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyAggregate {
    Sum,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AggregateObject {
    #[serde(default)]
    items: Combine,
    #[serde(default)]
    item_weights: BTreeMap<String, f64>,
    #[serde(default)]
    stages: StagesAgg,
    #[serde(default)]
    weights: BTreeMap<String, f64>,
    #[serde(default)]
    normalize: bool,
}

impl From<RawAggregate> for Aggregate {
    fn from(r: RawAggregate) -> Self {
        match r {
            RawAggregate::Legacy(LegacyAggregate::Sum) => Aggregate::default(),
            RawAggregate::Object(o) => Aggregate {
                items: o.items,
                item_weights: o.item_weights,
                stages: o.stages,
                weights: o.weights,
                normalize: o.normalize,
            },
        }
    }
}

/// What the scores mean and how to show them (docs/plugins.md §9). Plain
/// data; its texts come from uploaders and are shown as plain text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<ScoreFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<ScoreFormat>,
}

impl Display {
    pub fn is_empty(&self) -> bool {
        self.stage.is_none() && self.total.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreFormat {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default = "two")]
    pub decimals: u8,
    #[serde(default)]
    pub format: NumFormat,
}

fn two() -> u8 {
    2
}

impl Default for ScoreFormat {
    fn default() -> Self {
        ScoreFormat {
            name: String::new(),
            unit: String::new(),
            direction: Direction::Higher,
            min: None,
            max: None,
            decimals: 2,
            format: NumFormat::Number,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Higher is better.
    #[default]
    Higher,
    Lower,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumFormat {
    #[default]
    Number,
    /// × 100, with `%`.
    Percent,
    /// `score/max` (tests passed / tests).
    Fraction,
}

impl ScoreFormat {
    /// The value as text: `4458.56 分`, `77.5%`, `27/30`. The same rules
    /// as the web page (web/src/stats.ts `fmtValue`).
    pub fn fmt(&self, v: f64, max: Option<f64>) -> String {
        let d = usize::from(self.decimals.min(6));
        let num = |x: f64| format!("{x:.d$}");
        let s = match (self.format, max) {
            (NumFormat::Percent, _) => return format!("{:.d$}%", v * 100.0),
            (NumFormat::Fraction, Some(m)) => format!("{}/{}", num(v), num(m)),
            _ => num(v),
        };
        if self.unit.is_empty() {
            s
        } else {
            format!("{s} {}", self.unit)
        }
    }

    fn check(&self) -> Result<(), String> {
        let text_ok = |s: &str, n: usize| s.chars().count() <= n && !s.chars().any(char::is_control);
        if !text_ok(&self.name, 40) || !text_ok(&self.unit, 10) {
            return Err("display: name ≤ 40, unit ≤ 10 characters, no control characters".into());
        }
        if self.decimals > 6 {
            return Err("display: decimals must be 0–6".into());
        }
        if [self.min, self.max].iter().flatten().any(|x| !x.is_finite()) {
            return Err("display: min / max must be finite".into());
        }
        Ok(())
    }
}

/// `model` of docs/plugins.md §10 (P3; parsed only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDecl {
    #[serde(default)]
    pub interactive: ModelUse,
    #[serde(default)]
    pub scorer: ModelUse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_requests: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUse {
    #[default]
    None,
    Optional,
    Required,
}

/// What a manifest records about how it was scored, so later changes to
/// the taskset never change how an old evaluation reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scoring {
    pub aggregate: Aggregate,
    /// Resolved: both formats always present.
    pub display: Display,
    pub plugins: Vec<PluginVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginVersion {
    pub kind: String,
    pub name: String,
    pub version: String,
}

/// The display of evaluations that predate `scoring`: test counts per
/// stage, the ratio total as a percentage.
pub fn legacy_display() -> Display {
    Display {
        stage: Some(ScoreFormat {
            decimals: 0,
            format: NumFormat::Fraction,
            ..ScoreFormat::default()
        }),
        total: Some(ScoreFormat {
            decimals: 1,
            format: NumFormat::Percent,
            min: Some(0.0),
            max: Some(1.0),
            ..ScoreFormat::default()
        }),
    }
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
    #[error("taskset: schema must be 1 or 2")]
    Schema,
    #[error("taskset: {0}")]
    Scoring(String),
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

    /// `aggregate` and `display` checks: weights finite and naming existing
    /// stages, display texts short and plain.
    fn check_scoring(&self) -> Result<(), String> {
        let a = &self.aggregate;
        if a.weights.values().chain(a.item_weights.values()).any(|w| !w.is_finite()) {
            return Err("aggregate: weights must be finite".into());
        }
        if let Some(id) = a.weights.keys().find(|k| !self.stages.iter().any(|s| &s.id == *k)) {
            return Err(format!("aggregate: weight for unknown stage {id:?}"));
        }
        if a.item_weights.len() > crate::score::MAX_ITEMS {
            return Err("aggregate: too many item weights".into());
        }
        for f in [&self.display.stage, &self.display.total].into_iter().flatten() {
            f.check()?;
        }
        Ok(())
    }

    /// The `scoring` snapshot a manifest records: the aggregate, the
    /// display with defaults filled in, the scorer's version.
    ///
    /// Defaults (docs/plugins.md §9): stages show `passed/total` when
    /// every stage declares a test count (`expected_total`), else a number;
    /// a `ratio` total shows as a percentage, others as a number.
    pub fn scoring(&self) -> Scoring {
        let legacy = legacy_display();
        let counted = self.stages.iter().all(|s| s.expected_total.is_some());
        let stage = self.display.stage.clone().unwrap_or_else(|| {
            if counted {
                legacy.stage.clone().unwrap_or_default()
            } else {
                ScoreFormat::default()
            }
        });
        let total = self.display.total.clone().unwrap_or_else(|| {
            if self.aggregate.stages == StagesAgg::Ratio {
                legacy.total.clone().unwrap_or_default()
            } else {
                ScoreFormat {
                    name: stage.name.clone(),
                    unit: stage.unit.clone(),
                    direction: stage.direction,
                    decimals: stage.decimals,
                    ..ScoreFormat::default()
                }
            }
        });
        let version = self.scorer.version.clone().unwrap_or_else(|| {
            SCORER_VERSIONS
                .iter()
                .find(|(n, _)| *n == self.scorer.name)
                .map_or("unknown", |(_, v)| v)
                .to_owned()
        });
        Scoring {
            aggregate: self.aggregate.clone(),
            display: Display {
                stage: Some(stage),
                total: Some(total),
            },
            plugins: vec![PluginVersion {
                kind: "scorer".into(),
                name: self.scorer.name.clone(),
                version,
            }],
        }
    }

    /// Registration check against the platform's per-run limit.
    pub fn validate(&self, max_total_s: u64) -> Result<(), TaskSetError> {
        if !(1..=2).contains(&self.schema) {
            return Err(TaskSetError::Schema);
        }
        self.check_scoring().map_err(TaskSetError::Scoring)?;
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
