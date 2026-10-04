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
use crate::plugins::Kind;

/// `name` or `name@version` of a scorer reference.
pub fn scorer_spec(s: &ScorerRef) -> String {
    match &s.version {
        Some(v) => format!("{}@{v}", s.name),
        None => s.name.clone(),
    }
}

/// Platform limit on the summed stage time of one run (all stages run back
/// to back on one machine, inside a 6 h GitHub job).
pub const MAX_TOTAL_TIME_S: u64 = 18_000;

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
    /// Model use while scoring (docs/plugins.md §10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelDecl>,
    /// Declared wall clock of a whole run; at least the sum of the stage
    /// limits and at most [`MAX_TOTAL_TIME_S`].
    pub total_time_limit_s: u64,
    pub stages: Vec<Stage>,
}

/// Which scorer grades the checkpoints, e.g. `playwright`: a name in the
/// plugin registry (`plugins.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScorerRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

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
        let text_ok =
            |s: &str, n: usize| s.chars().count() <= n && !s.chars().any(char::is_control);
        if !text_ok(&self.name, 40) || !text_ok(&self.unit, 10) {
            return Err("display: name ≤ 40, unit ≤ 10 characters, no control characters".into());
        }
        if self.decimals > 6 {
            return Err("display: decimals must be 0–6".into());
        }
        if [self.min, self.max]
            .iter()
            .flatten()
            .any(|x| !x.is_finite())
        {
            return Err("display: min / max must be finite".into());
        }
        Ok(())
    }
}

/// A model name as the meter and the plugins take it.
pub fn model_name_ok(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 80
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

/// `model` of docs/plugins.md §10: which slots of the scoring job may use
/// the submitter's model, through the meter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// The packager plugin that turns the work dir into the stage output
    /// (`web-app`, `files`). Tasksets written before the plugin registry
    /// call it `output`.
    #[serde(alias = "output")]
    pub packager: String,
    /// Options for the packager, e.g. `{"require": ["observer.project.json"]}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packager_options: Option<serde_json::Value>,
    /// The producing runner; `None` = `workdir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// This stage's scorer when it differs from the taskset's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scorer: Option<ScorerRef>,
    /// Parameters for the scorer (`--options FILE`), e.g. how many times a
    /// judge model grades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scorer_options: Option<serde_json::Value>,
    /// The interactive runner (slot 3), run in the scoring job before the
    /// scorer, in both modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive: Option<InteractiveRef>,
    pub time_limit_s: u64,
    /// Number of tests the scorer is expected to report. A mismatch is
    /// flagged rather than silently summed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_total: Option<u32>,
}

/// An interactive runner and its wall clock (docs/plugins.md §4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractiveRef {
    pub name: String,
    pub time_limit_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<serde_json::Value>,
}

/// The default producing runner.
pub const DEFAULT_RUNNER: &str = "workdir";

/// Largest `packager_options`, as JSON.
pub const MAX_PLUGIN_OPTIONS_BYTES: usize = 4096;

impl Stage {
    pub fn runner_name(&self) -> &str {
        self.runner.as_deref().unwrap_or(DEFAULT_RUNNER)
    }

    /// The scorer of this stage: its own, else the taskset's.
    pub fn scorer_ref<'a>(&'a self, ts: &'a TaskSet) -> &'a ScorerRef {
        self.scorer.as_ref().unwrap_or(&ts.scorer)
    }
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
    #[error("taskset: {0}")]
    Plugin(String),
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
        if a.weights
            .values()
            .chain(a.item_weights.values())
            .any(|w| !w.is_finite())
        {
            return Err("aggregate: weights must be finite".into());
        }
        if let Some(id) = a
            .weights
            .keys()
            .find(|k| !self.stages.iter().any(|s| &s.id == *k))
        {
            return Err(format!("aggregate: weight for unknown stage {id:?}"));
        }
        if a.item_weights.len() > crate::score::MAX_ITEMS {
            return Err("aggregate: too many item weights".into());
        }
        for f in [&self.display.stage, &self.display.total]
            .into_iter()
            .flatten()
        {
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
        Scoring {
            aggregate: self.aggregate.clone(),
            display: Display {
                stage: Some(stage),
                total: Some(total),
            },
            plugins: self.plugin_versions(),
        }
    }

    /// Every plugin this taskset uses, scorers first, each once, with the
    /// registered version (or the one the taskset pinned).
    pub fn plugin_versions(&self) -> Vec<PluginVersion> {
        let reg = crate::plugins::registry();
        let mut out: Vec<PluginVersion> = Vec::new();
        let mut add = |kind: Kind, r: &str| {
            let (name, pinned) = match r.split_once('@') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (r, None),
            };
            let version = pinned
                .or_else(|| reg.get(kind, name).map(|p| p.version.clone()))
                .unwrap_or_else(|| "unknown".into());
            let v = PluginVersion {
                kind: kind.as_str().into(),
                name: name.into(),
                version,
            };
            if !out.contains(&v) {
                out.push(v);
            }
        };
        for s in &self.stages {
            let sc = s.scorer_ref(self);
            match &sc.version {
                Some(v) => add(Kind::Scorer, &format!("{}@{v}", sc.name)),
                None => add(Kind::Scorer, &sc.name),
            }
        }
        for s in &self.stages {
            if let Some(i) = &s.interactive {
                add(Kind::Runner, &i.name);
            }
        }
        for s in &self.stages {
            add(Kind::Runner, s.runner_name());
            add(Kind::Packager, &s.packager);
        }
        out
    }

    /// Every plugin reference resolves in the registry with the right kind;
    /// producing runners are not interactive; a scorer accepts the stage's
    /// packager; plugin options are small JSON objects.
    pub fn check_plugins(&self) -> Result<(), String> {
        let reg = crate::plugins::registry();
        for s in &self.stages {
            let at = |e: String| format!("stage {}: {e}", s.id);
            let sc = s.scorer_ref(self);
            let scorer = reg.resolve(Kind::Scorer, &scorer_spec(sc)).map_err(at)?;
            let runner = reg.resolve(Kind::Runner, s.runner_name()).map_err(at)?;
            if runner.interactive {
                return Err(at(format!("runner {} is interactive", runner.name)));
            }
            let packager = reg.resolve(Kind::Packager, &s.packager).map_err(at)?;
            if !scorer.accepts.is_empty() && !scorer.accepts.contains(&packager.name) {
                return Err(at(format!(
                    "scorer {} needs packager {}",
                    scorer.name,
                    scorer.accepts.join(" or ")
                )));
            }
            for (what, o) in [
                ("packager_options", &s.packager_options),
                ("scorer_options", &s.scorer_options),
                (
                    "interactive.options",
                    &s.interactive.as_ref().and_then(|i| i.options.clone()),
                ),
            ] {
                if let Some(o) = o
                    && (!o.is_object() || o.to_string().len() > MAX_PLUGIN_OPTIONS_BYTES)
                {
                    return Err(at(format!(
                        "{what} must be a JSON object of at most {MAX_PLUGIN_OPTIONS_BYTES} bytes"
                    )));
                }
            }
            let model = self.model.clone().unwrap_or_default();
            if let Some(i) = &s.interactive {
                let r = reg.resolve(Kind::Runner, &i.name).map_err(at)?;
                if !r.interactive {
                    return Err(at(format!("runner {} is not interactive", r.name)));
                }
                if i.time_limit_s == 0 || i.time_limit_s > MAX_TOTAL_TIME_S {
                    return Err(at("interactive.time_limit_s must be 1..=18000".into()));
                }
                if model.interactive != ModelUse::None && !r.model {
                    return Err(at(format!("runner {} cannot be given a model", r.name)));
                }
            }
            if model.scorer != ModelUse::None && !scorer.model {
                return Err(at(format!(
                    "scorer {} cannot be given a model",
                    scorer.name
                )));
            }
        }
        if let Some(m) = &self.model {
            if m.interactive != ModelUse::None
                && !self.stages.iter().any(|s| s.interactive.is_some())
            {
                return Err(
                    "model.interactive is set but no stage has an interactive runner".into(),
                );
            }
            if let Some(n) = &m.name
                && !model_name_ok(n)
            {
                return Err("model.name: at most 80 characters of [A-Za-z0-9._:/-]".into());
            }
            if m.max_requests == Some(0) || m.max_tokens == Some(0) {
                return Err("model.max_requests / max_tokens must be > 0".into());
            }
        }
        // The scoring job of one replica runs every stage's interactive
        // run back to back.
        let interactive_s: u64 = self
            .stages
            .iter()
            .filter_map(|s| s.interactive.as_ref())
            .map(|i| i.time_limit_s)
            .sum();
        if interactive_s > MAX_TOTAL_TIME_S {
            return Err(format!(
                "interactive time limits add up to {interactive_s}s, more than {MAX_TOTAL_TIME_S}s"
            ));
        }
        Ok(())
    }

    /// Does any stage's slot use a model (`interactive` or `scorer`)?
    pub fn model_use(&self) -> (ModelUse, ModelUse) {
        let m = self.model.clone().unwrap_or_default();
        (m.interactive, m.scorer)
    }

    /// The extra rule for uploaded tasksets: every plugin they use is
    /// offered to users (`"user": true` in the registry).
    pub fn check_user_plugins(&self) -> Result<(), String> {
        self.check_plugins()?;
        let reg = crate::plugins::registry();
        for s in &self.stages {
            let sc = s.scorer_ref(self);
            let interactive = s
                .interactive
                .as_ref()
                .map(|i| (Kind::Runner, i.name.clone()));
            for (kind, r) in [
                (Kind::Scorer, scorer_spec(sc)),
                (Kind::Runner, s.runner_name().to_owned()),
                (Kind::Packager, s.packager.clone()),
            ]
            .into_iter()
            .chain(interactive)
            {
                let p = reg.resolve(kind, &r)?;
                // The default runner needs no review of its own: it never
                // sees test material.
                if !p.user && !(kind == Kind::Runner && p.name == DEFAULT_RUNNER) {
                    return Err(format!(
                        "{} {:?} is not available for uploaded tasksets",
                        kind.as_str(),
                        p.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// Registration check against the platform's per-run limit.
    pub fn validate(&self, max_total_s: u64) -> Result<(), TaskSetError> {
        if !(1..=2).contains(&self.schema) {
            return Err(TaskSetError::Schema);
        }
        self.check_scoring().map_err(TaskSetError::Scoring)?;
        self.check_plugins().map_err(TaskSetError::Plugin)?;
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
        assert_eq!(t.stages[0].packager, "web-app");
        assert_eq!(t.stages[0].runner_name(), "workdir");
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
        let mut t = base.clone();
        t.stages[0].time_limit_s = 0;
        assert!(matches!(
            t.validate(1 << 20),
            Err(TaskSetError::TimeLimit(_))
        ));
        // Plugins: unknown names, wrong kinds, a packager the scorer
        // cannot take, a pinned version this platform does not have.
        for (f, why) in [
            (
                Box::new(|t: &mut TaskSet| t.stages[0].packager = "web_app".into())
                    as Box<dyn Fn(&mut TaskSet)>,
                "unknown packager",
            ),
            (
                Box::new(|t: &mut TaskSet| t.stages[0].packager = "files".into()),
                "playwright takes web-app",
            ),
            (
                Box::new(|t: &mut TaskSet| t.stages[0].runner = Some("playwright".into())),
                "scorer as runner",
            ),
            (
                Box::new(|t: &mut TaskSet| t.scorer.version = Some("7".into())),
                "version",
            ),
            (
                Box::new(|t: &mut TaskSet| {
                    t.stages[1].scorer = Some(ScorerRef {
                        name: "nope".into(),
                        version: None,
                    })
                }),
                "stage scorer",
            ),
            (
                Box::new(|t: &mut TaskSet| {
                    t.stages[0].packager_options = Some(serde_json::json!([1]))
                }),
                "options not an object",
            ),
        ] {
            let mut t = base.clone();
            f(&mut t);
            assert!(
                matches!(t.validate(1 << 20), Err(TaskSetError::Plugin(_))),
                "{why}"
            );
        }
        // Models only for plugins that may have one; interactive runners
        // only in the interactive slot.
        let astro = |t: &mut TaskSet| {
            t.scorer.name = "astro-survey".into();
            for s in &mut t.stages {
                s.packager = "files".into();
                s.expected_total = None;
                s.interactive = Some(InteractiveRef {
                    name: "astro-v4".into(),
                    time_limit_s: 1500,
                    options: None,
                });
            }
        };
        let mut t = base.clone();
        astro(&mut t);
        t.model = Some(ModelDecl {
            interactive: ModelUse::Optional,
            ..ModelDecl::default()
        });
        t.validate(1 << 20).unwrap();
        t.check_user_plugins().unwrap();
        for (f, why) in [
            (
                Box::new(|t: &mut TaskSet| t.model.as_mut().unwrap().scorer = ModelUse::Required)
                    as Box<dyn Fn(&mut TaskSet)>,
                "astro-survey has no model",
            ),
            (
                Box::new(|t: &mut TaskSet| {
                    t.stages[0].interactive.as_mut().unwrap().name = "workdir".into()
                }),
                "workdir is not interactive",
            ),
            (
                Box::new(|t: &mut TaskSet| t.stages[0].runner = Some("astro-v4".into())),
                "interactive runner in slot 1",
            ),
            (
                Box::new(|t: &mut TaskSet| t.model.as_mut().unwrap().name = Some("a b".into())),
                "model name",
            ),
            (
                Box::new(|t: &mut TaskSet| {
                    t.stages[0].interactive.as_mut().unwrap().time_limit_s = 18_000
                }),
                "interactive time",
            ),
        ] {
            let mut t2 = t.clone();
            f(&mut t2);
            assert!(
                matches!(t2.validate(1 << 20), Err(TaskSetError::Plugin(_))),
                "{why}"
            );
        }
        // Playwright runs taskset code: never a model.
        let mut t = base.clone();
        t.model = Some(ModelDecl {
            scorer: ModelUse::Optional,
            ..ModelDecl::default()
        });
        assert!(matches!(t.validate(1 << 20), Err(TaskSetError::Plugin(_))));
        // Uploaded tasksets: only plugins marked `user`.
        base.check_user_plugins().unwrap();
        let mut t = base;
        t.scorer.name = "arcbench-official".into();
        t.validate(1 << 20).unwrap();
        assert!(t.check_user_plugins().is_err());
    }

    /// The repository's tasksets parse, validate, and snapshot as intended.
    #[test]
    fn builtin_tasksets() {
        let hello: TaskSet =
            serde_json::from_str(include_str!("../../../tasksets/hello-world/taskset.json"))
                .unwrap();
        hello.validate(MAX_TOTAL_TIME_S).unwrap();
        let sc = hello.scoring();
        assert_eq!(sc.aggregate.stages, StagesAgg::Ratio);
        assert_eq!(
            sc.display.stage.as_ref().unwrap().format,
            NumFormat::Fraction
        );
        assert_eq!(sc.display.total.as_ref().unwrap().fmt(1.0, None), "100.0%");
        assert_eq!(sc.plugins[0].version, "1");
        assert_eq!(
            sc.plugins
                .iter()
                .map(|p| format!("{}:{}@{}", p.kind, p.name, p.version))
                .collect::<Vec<_>>(),
            [
                "scorer:playwright@1",
                "runner:workdir@1",
                "packager:web-app@1"
            ]
        );

        let astro: TaskSet = serde_json::from_str(include_str!(
            "../../../tasksets/astro-practice/taskset.json"
        ))
        .unwrap();
        astro.validate(MAX_TOTAL_TIME_S).unwrap();
        let sc = astro.scoring();
        assert_eq!(sc.aggregate.stages, StagesAgg::Sum);
        let stage = sc.display.stage.as_ref().unwrap();
        assert_eq!(stage.name, "观测得分");
        assert_eq!(stage.fmt(4458.556163, None), "4458.56 分");
        assert_eq!(sc.plugins[0].version, "2");
        // Serialized and read back: the object form survives.
        let back: TaskSet = serde_json::from_str(&serde_json::to_string(&astro).unwrap()).unwrap();
        assert_eq!(back, astro);

        // Every taskset in the repository, unchanged, still validates.
        for raw in [
            include_str!("../../../tasksets/arcbench-github/taskset.json"),
            include_str!("../../../tasksets/arcbench-github-official/taskset.json"),
            include_str!("../../../tasksets/math-proof-demo/taskset.json"),
        ] {
            let t: TaskSet = serde_json::from_str(raw).unwrap();
            t.validate(MAX_TOTAL_TIME_S).unwrap();
        }
    }

    #[test]
    fn scoring_checks() {
        let mut t: TaskSet = serde_json::from_str(&github()).unwrap();
        t.schema = 2;
        t.aggregate =
            serde_json::from_str(r#"{"stages":"weighted","weights":{"stage-9":1}}"#).unwrap();
        assert!(matches!(t.validate(1 << 20), Err(TaskSetError::Scoring(_))));
        t.aggregate =
            serde_json::from_str(r#"{"stages":"weighted","weights":{"stage-2":1}}"#).unwrap();
        t.validate(1 << 20).unwrap();
        t.display.stage = Some(ScoreFormat {
            name: "x\u{7}".into(),
            ..ScoreFormat::default()
        });
        assert!(matches!(t.validate(1 << 20), Err(TaskSetError::Scoring(_))));
        t.schema = 3;
        assert_eq!(t.validate(1 << 20), Err(TaskSetError::Schema));
        assert!(serde_json::from_str::<Aggregate>(r#""mean""#).is_err());
        assert!(serde_json::from_str::<Aggregate>(r#"{"stage":"sum"}"#).is_err());
        let f = ScoreFormat {
            format: NumFormat::Fraction,
            decimals: 0,
            ..ScoreFormat::default()
        };
        assert_eq!(f.fmt(27.0, Some(30.0)), "27/30");
    }
}
