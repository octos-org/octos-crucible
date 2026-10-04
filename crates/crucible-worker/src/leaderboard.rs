//! Leaderboards (`GET /leaderboard`, `GET /leaderboard/:taskset`).
//!
//! Only evals submitted with `score_public = true` whose results are `done`
//! and scored are candidates (the SQL in `store` selects nothing else). For
//! each (owner, agent name) the best one counts, by the direction of the
//! taskset's display (`higher`: the largest total, `lower`: the smallest);
//! ties go to the earlier eval, and equal totals share a rank.

use crucible_core::Manifest;
use crucible_core::taskset::{Direction, ScoreFormat};
use serde::{Deserialize, Serialize};

/// At most this many entries (one per owner and agent) are shown.
pub const MAX_ENTRIES: usize = 100;
/// Newest public evals considered per taskset.
pub const MAX_CANDIDATES: usize = 2000;

/// One public, done, scored eval (light columns only).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub eval_id: String,
    pub owner_id: u64,
    pub login: String,
    pub agent: String,
    pub agent_version: String,
    pub model: Option<String>,
    pub total_score: f64,
    pub created_at: String,
    pub created_s: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageCell {
    pub stage: String,
    /// Mean over the replicas that scored this stage; `None` if none did.
    pub score: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub rank: u32,
    pub login: String,
    pub agent: String,
    pub agent_version: String,
    pub model: Option<String>,
    pub total_score: f64,
    pub stages: Vec<StageCell>,
    pub replicas: u32,
    /// Mean per replica of the summed stage wall times.
    pub wall_s: Option<f64>,
    /// Mean per replica of the summed stage costs; `None` when unknown.
    pub cost_usd: Option<f64>,
    pub created_at: String,
    pub eval_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub taskset: String,
    pub direction: Direction,
    /// How totals are shown (the newest public eval's snapshot); absent:
    /// old evals, a 0–1 ratio shown as a percentage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ScoreFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_display: Option<ScoreFormat>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardInfo {
    pub taskset: String,
    /// Public, done evals on it.
    pub evals: u64,
    pub latest_at: String,
}

/// `a` is better than `b`: by the direction, then the earlier eval.
fn better(a: &Candidate, b: &Candidate, dir: Direction) -> bool {
    let by_score = match dir {
        Direction::Higher => a.total_score.total_cmp(&b.total_score),
        Direction::Lower => b.total_score.total_cmp(&a.total_score),
    };
    by_score
        .then_with(|| b.created_s.cmp(&a.created_s))
        .then_with(|| b.eval_id.cmp(&a.eval_id))
        .is_gt()
}

/// The best candidate of each (owner, agent), best first, with ranks
/// (equal totals share one), at most [`MAX_ENTRIES`].
pub fn rank(cands: Vec<Candidate>, dir: Direction) -> Vec<(u32, Candidate)> {
    let mut best: Vec<Candidate> = Vec::new();
    for c in cands {
        match best
            .iter_mut()
            .find(|b| b.owner_id == c.owner_id && b.agent == c.agent)
        {
            Some(b) if better(&c, b, dir) => *b = c,
            Some(_) => {}
            None => best.push(c),
        }
    }
    best.sort_by(|a, b| {
        if better(a, b, dir) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Greater
        }
    });
    best.truncate(MAX_ENTRIES);
    let mut out: Vec<(u32, Candidate)> = Vec::with_capacity(best.len());
    for (i, c) in best.into_iter().enumerate() {
        let r = match out.last() {
            Some((r, p)) if p.total_score == c.total_score => *r,
            _ => i as u32 + 1,
        };
        out.push((r, c));
    }
    out
}

/// Per-stage means, replica count, time and cost, from the manifest.
pub fn entry(rank: u32, c: Candidate, m: Option<&Manifest>) -> Entry {
    let mut stages: Vec<(String, Vec<f64>, Option<f64>)> = Vec::new();
    let (mut walls, mut costs) = (Vec::new(), Vec::new());
    let reps = m.map_or(&[][..], |m| &m.replicas[..]);
    for r in reps {
        for s in &r.stages {
            let i = match stages.iter().position(|(n, _, _)| n == &s.stage) {
                Some(i) => i,
                None => {
                    stages.push((s.stage.clone(), Vec::new(), None));
                    stages.len() - 1
                }
            };
            if let Some((v, max)) = s.score.as_ref().and_then(|x| x.value()) {
                stages[i].1.push(v);
                stages[i].2 = stages[i].2.or(max);
            }
        }
        let sum = |f: &dyn Fn(&crucible_core::manifest::StageEntry) -> Option<f64>| {
            r.stages.iter().map(f).sum::<Option<f64>>()
        };
        if !r.stages.is_empty() {
            walls.extend(sum(&|s| s.wall_s));
            costs.extend(sum(&|s| s.cost_usd));
        }
    }
    let mean = |xs: &[f64]| (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64);
    let all_costs = costs.len() == reps.iter().filter(|r| !r.stages.is_empty()).count();
    Entry {
        rank,
        login: c.login,
        agent: c.agent,
        agent_version: c.agent_version,
        model: c.model,
        total_score: c.total_score,
        stages: stages
            .into_iter()
            .map(|(stage, v, max)| StageCell {
                stage,
                score: mean(&v),
                max,
            })
            .collect(),
        replicas: reps.len() as u32,
        wall_s: mean(&walls),
        cost_usd: if all_costs { mean(&costs) } else { None },
        created_at: c.created_at,
        eval_id: c.eval_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, owner: u64, agent: &str, score: f64, at: u64) -> Candidate {
        Candidate {
            eval_id: id.into(),
            owner_id: owner,
            login: format!("u{owner}"),
            agent: agent.into(),
            agent_version: "1".into(),
            model: None,
            total_score: score,
            created_at: String::new(),
            created_s: at,
        }
    }

    fn ids(v: &[(u32, Candidate)]) -> Vec<(u32, &str)> {
        v.iter().map(|(r, c)| (*r, c.eval_id.as_str())).collect()
    }

    #[test]
    fn best_per_owner_and_agent_by_direction() {
        let cands = || {
            vec![
                c("a1", 1, "x", 0.5, 10),
                c("a2", 1, "x", 0.9, 20),
                c("a3", 1, "y", 0.7, 30), // another agent of the same owner
                c("b1", 2, "x", 0.6, 40), // same agent name, another owner
            ]
        };
        assert_eq!(
            ids(&rank(cands(), Direction::Higher)),
            [(1, "a2"), (2, "a3"), (3, "b1")]
        );
        assert_eq!(
            ids(&rank(cands(), Direction::Lower)),
            [(1, "a1"), (2, "b1"), (3, "a3")]
        );
    }

    #[test]
    fn ties_share_a_rank_and_earlier_wins() {
        let r = rank(
            vec![
                c("late", 1, "x", 1.0, 50),
                c("early", 1, "x", 1.0, 5), // same owner and agent: earlier kept
                c("other", 2, "x", 1.0, 30),
                c("third", 3, "x", 0.2, 1),
            ],
            Direction::Higher,
        );
        assert_eq!(ids(&r), [(1, "early"), (1, "other"), (3, "third")]);
    }

    #[test]
    fn entry_from_manifest() {
        let usage = r#""usage":{"requests":0,"prompt_tokens":0,"cached_tokens":0,"completion_tokens":0,"reasoning_tokens":0}"#;
        let st = |s: &str, sc: f64, w: f64, cost: &str| {
            format!(
                r#"{{"stage":"{s}","score":{{"status":"scored","score":{sc},"max":2}},"wall_s":{w},"cost_usd":{cost},{usage}}}"#
            )
        };
        let raw = format!(
            r#"{{"schema":2,"eval_id":"e","created_at":"t","taskset":"t","agent":{{"name":"a","version":"1"}},"model":"m","replicas":[{{"replica":1,"stages":[{},{}]}},{{"replica":2,"stages":[{},{}]}}]}}"#,
            st("s1", 1.0, 10.0, "0.5"),
            st("s2", 2.0, 20.0, "0.5"),
            st("s1", 2.0, 30.0, "1.0"),
            st("s2", 0.0, 40.0, "null"),
        );
        let m: Manifest = serde_json::from_str(&raw).unwrap();
        let e = entry(1, c("e", 1, "a", 1.0, 0), Some(&m));
        assert_eq!(e.replicas, 2);
        assert_eq!(
            e.stages,
            [
                StageCell {
                    stage: "s1".into(),
                    score: Some(1.5),
                    max: Some(2.0)
                },
                StageCell {
                    stage: "s2".into(),
                    score: Some(1.0),
                    max: Some(2.0)
                },
            ]
        );
        assert_eq!(e.wall_s, Some(50.0));
        // One replica's price is unknown: no cost.
        assert_eq!(e.cost_usd, None);
    }
}
