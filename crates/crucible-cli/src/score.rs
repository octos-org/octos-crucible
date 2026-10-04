//! `crucible score`: score each stage checkpoint with the taskset's scorer.
//!
//! Input layout (same as the publish job's): `<results>/<replica>/<stage>/
//! checkpoint.sealed` (plus `logs.sealed` when the stage ran). Per stage the
//! tests blob is fetched and opened into a temporary directory that is
//! deleted as soon as that stage is scored; each checkpoint is opened into a
//! temporary file the same way.
//!
//! Isolation (docs/scorer-contract.md §7): in the workflows the tests run
//! on a machine that holds no secret. `handoff` (on the machine with the
//! platform key) re-seals the selected tests blobs and checkpoints to a
//! fresh one-run key; `score` then runs elsewhere with only that key, read
//! from `TestsFrom::Dir`, and the scorer process never sees it. The scorer (`scorers/<name>/score.sh`, see
//! docs/scorer-contract.md) runs with `--visibility hidden`, so its
//! result.json holds only the score, fixed-name items and a fixed detail text:
//! nothing of the tests leaves this job.
//!
//! Output: `<out>/<replica>/<stage>/score.json`, always result v2 (an old
//! format result is converted), normalised for the manifest:
//! - only items: score / max from them (the taskset's `aggregate.items`);
//! - no score, or out of bounds: `error` (system);
//! - a counted stage (`expected_total`) scored with no test results (build
//!   failed, app never ready): score 0, max = `expected_total`;
//! - a test count that differs from `expected_total`: `error` (flagged,
//!   not summed).

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use crucible_core::score::ErrorKind;
use crucible_core::taskset::{Aggregate, Stage};
use crucible_core::{ScoreResult, TaskSet};
use crucible_crypto::PrivateKey;

use crate::keys::Store;
use crate::zipdir::{self, ExtractLimits};

/// Limits for unpacking a tests blob.
pub const TESTS_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 5_000,
    max_bytes: 256 << 20,
};

/// Where the tests of a stage come from.
pub enum TestsFrom<'a> {
    /// The taskset's tests blob in the store.
    Store(&'a Store),
    /// `<dir>/<stage id>.sealed`, written by `handoff`.
    Dir(&'a Path),
}

pub struct ScoreOpts<'a> {
    pub taskset: &'a TaskSet,
    /// Indices into `taskset.stages` to score.
    pub stages: Vec<usize>,
    pub results: &'a Path,
    pub out: &'a Path,
    pub tests: TestsFrom<'a>,
    pub keys: &'a [PrivateKey],
    /// `score.sh` of the taskset's scorer.
    pub scorer: &'a Path,
    /// Environment variables never passed to the scorer (the key names).
    pub scrub_env: &'a [String],
}

/// Normalise a scorer result for the manifest: [`ScoreResult::finish`]
/// (score from items, bounds), then the stage's declared test count.
pub fn normalise(r: ScoreResult, expected_total: Option<u32>, agg: &Aggregate) -> ScoreResult {
    let mut r = r.finish(agg);
    if !r.status.is_scored() {
        return r;
    }
    if let Some(e) = expected_total {
        let e = f64::from(e);
        match r.max {
            None | Some(0.0) => {
                r.score = Some(0.0);
                r.max = Some(e);
            }
            Some(m) if m != e => {
                return ScoreResult {
                    visibility: r.visibility,
                    task_id: r.task_id,
                    submission_id: r.submission_id,
                    ..ScoreResult::error(
                        ErrorKind::System,
                        format!("scorer reported {m} tests, the taskset expects {e}"),
                    )
                };
            }
            _ => {}
        }
    }
    r
}

fn system_error(detail: impl Into<String>) -> ScoreResult {
    ScoreResult {
        visibility: Some("hidden".into()),
        ..ScoreResult::error(ErrorKind::System, detail)
    }
}

/// Scored 0: the agent produced nothing to score.
fn zero(detail: impl Into<String>) -> ScoreResult {
    ScoreResult {
        visibility: Some("hidden".into()),
        ..ScoreResult::scored(0.0, None, detail)
    }
}

/// Numeric replica directories under `results`, sorted.
fn replicas(results: &Path) -> Result<Vec<u32>> {
    let mut out: Vec<u32> = std::fs::read_dir(results)
        .with_context(|| format!("reading {}", results.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    out.sort();
    Ok(out)
}

/// Run the scorer on one opened checkpoint.
fn run_scorer(
    scorer: &Path,
    artifact: &Path,
    tests: &Path,
    stage: &Stage,
    scrub_env: &[String],
) -> ScoreResult {
    let work = match tempfile::tempdir() {
        Ok(w) => w,
        Err(e) => return system_error(format!("temp dir: {e}")),
    };
    let out = work.path().join("result.json");
    let mut cmd = Command::new("bash");
    for k in scrub_env {
        cmd.env_remove(k);
    }
    let status = cmd
        .arg(scorer)
        .arg("--artifact")
        .arg(artifact)
        .arg("--tests")
        .arg(tests)
        .arg("--out")
        .arg(&out)
        .args(["--visibility", "hidden", "--task-id", &stage.id])
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) if s.code() == Some(2) => {
            return system_error("scorer refused its arguments (exit 2)");
        }
        Ok(s) => return system_error(format!("scorer wrote no result ({s})")),
        Err(e) => return system_error(format!("could not start the scorer: {e}")),
    }
    match std::fs::read(&out)
        .ok()
        .and_then(|b| serde_json::from_slice::<ScoreResult>(&b).ok())
    {
        Some(r) => r,
        None => system_error("scorer result.json is missing or malformed"),
    }
}

/// Score every replica's checkpoint of the selected stages.
pub async fn score(o: &ScoreOpts<'_>) -> Result<Vec<(u32, String, ScoreResult)>> {
    if !o.scorer.is_file() {
        bail!("scorer {} not found", o.scorer.display());
    }
    let reps = replicas(o.results)?;
    if reps.is_empty() {
        bail!("no replica directories under {}", o.results.display());
    }
    let mut done = Vec::new();
    for &i in &o.stages {
        let stage = &o.taskset.stages[i];
        // Which replicas have anything for this stage at all.
        let todo: Vec<u32> = reps
            .iter()
            .copied()
            .filter(|r| o.results.join(r.to_string()).join(&stage.id).is_dir())
            .collect();
        if todo.is_empty() {
            continue;
        }
        // The tests exist in clear only inside this scope.
        let tests = tempfile::tempdir()?;
        let plain = match o.tests {
            TestsFrom::Store(store) => store.get_sealed(&stage.tests_blob, o.keys).await?,
            TestsFrom::Dir(d) => {
                let p = d.join(format!("{}.sealed", stage.id));
                crucible_crypto::open(o.keys, &std::fs::read(&p)?)
                    .with_context(|| format!("opening {}", p.display()))?
            }
        };
        zipdir::safe_extract(Cursor::new(plain), tests.path(), TESTS_LIMITS)
            .with_context(|| format!("stage {} tests", stage.id))?;
        for r in todo {
            let sdir = o.results.join(r.to_string()).join(&stage.id);
            let ckpt = sdir.join("checkpoint.sealed");
            let result = if ckpt.is_file() {
                let app = tempfile::NamedTempFile::new()?;
                let zip = crucible_crypto::open(o.keys, &std::fs::read(&ckpt)?)
                    .with_context(|| format!("opening {}", ckpt.display()))?;
                std::fs::write(app.path(), &zip)?;
                drop(zip);
                run_scorer(o.scorer, app.path(), tests.path(), stage, o.scrub_env)
            } else {
                // The stage ran (it left logs) but produced nothing to score.
                zero("the stage left no checkpoint")
            };
            let result = normalise(result, stage.expected_total, &o.taskset.aggregate);
            let dir = o.out.join(r.to_string()).join(&stage.id);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("score.json"),
                serde_json::to_string_pretty(&result)? + "\n",
            )?;
            eprintln!(
                "r{r} {}: {:?} {:?}/{:?}",
                stage.id, result.status, result.score, result.max
            );
            done.push((r, stage.id.clone(), result));
        }
        drop(tests);
    }
    Ok(done)
}

/// Re-seal what the scoring machine needs to a fresh one-run key:
/// `<out>/tests/<stage>.sealed` (the stage's tests zip) and
/// `<out>/results/<replica>/<stage>/checkpoint.sealed` (an empty directory
/// when the stage ran but left no checkpoint). Nothing else of `results`
/// (logs, agent facts) is copied. Returns the one-run key.
pub async fn handoff(
    ts: &TaskSet,
    stages: &[usize],
    results: &Path,
    store: &Store,
    keys: &[PrivateKey],
    out: &Path,
) -> Result<PrivateKey> {
    let run_key = PrivateKey::generate();
    let to = run_key.public();
    let reps = replicas(results)?;
    std::fs::create_dir_all(out.join("tests"))?;
    for &i in stages {
        let stage = &ts.stages[i];
        let todo: Vec<u32> = reps
            .iter()
            .copied()
            .filter(|r| results.join(r.to_string()).join(&stage.id).is_dir())
            .collect();
        if todo.is_empty() {
            continue;
        }
        let plain = store.get_sealed(&stage.tests_blob, keys).await?;
        std::fs::write(
            out.join("tests").join(format!("{}.sealed", stage.id)),
            crucible_crypto::seal(&to, &plain)?,
        )?;
        drop(plain);
        for r in todo {
            let dir = out.join("results").join(r.to_string()).join(&stage.id);
            std::fs::create_dir_all(&dir)?;
            let ckpt = results
                .join(r.to_string())
                .join(&stage.id)
                .join("checkpoint.sealed");
            if ckpt.is_file() {
                let zip = crucible_crypto::open(keys, &std::fs::read(&ckpt)?)
                    .with_context(|| format!("opening {}", ckpt.display()))?;
                std::fs::write(
                    dir.join("checkpoint.sealed"),
                    crucible_crypto::seal(&to, &zip)?,
                )?;
            }
        }
    }
    Ok(run_key)
}

/// `<scores>/<replica>/<stage>/score.json`, if present.
pub fn read_score(scores: &Path, replica: u32, stage: &str) -> Result<Option<ScoreResult>> {
    let p: PathBuf = scores
        .join(replica.to_string())
        .join(stage)
        .join("score.json");
    match std::fs::read(&p) {
        Ok(b) => Ok(Some(
            serde_json::from_slice(&b).with_context(|| format!("parsing {}", p.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old(raw: &str) -> ScoreResult {
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn normalise_against_expected_total() {
        use crucible_core::ScoreStatus::{Error, Scored};
        let agg = Aggregate::default();
        let n = normalise(
            old(r#"{"status":"failed","passed":27,"total":30}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.status, n.score, n.max), (Scored, Some(27.0), Some(30.0)));
        // Build failed: no test ran, the stage still counts out of 30.
        let n = normalise(
            old(r#"{"status":"failed","passed":0,"total":0}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.score, n.max), (Some(0.0), Some(30.0)));
        // A pack that collected a different number of tests is flagged.
        let n = normalise(
            old(r#"{"status":"passed","passed":29,"total":29}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.status, n.score, n.max), (Error, None, None));
        assert!(n.detail.contains("expects 30"));
        let n = normalise(
            old(r#"{"status":"system_error","passed":3,"total":4}"#),
            Some(30),
            &agg,
        );
        assert_eq!((n.status, n.score), (Error, None));
        let n = normalise(
            old(r#"{"status":"passed","passed":4,"total":4}"#),
            None,
            &agg,
        );
        assert_eq!(
            (n.score, n.max, n.passed),
            (Some(4.0), Some(4.0), Some(true))
        );
        // A continuous score, negative, no max; written as v2.
        let n = normalise(
            old(r#"{"schema":2,"status":"scored","score":-3.25}"#),
            None,
            &agg,
        );
        assert_eq!((n.status, n.score, n.max), (Scored, Some(-3.25), None));
        let json = serde_json::to_string(&n).unwrap();
        assert!(json.contains(r#""schema":2"#) && !json.contains("total"));
    }

    #[tokio::test]
    async fn scores_with_a_stub_scorer() {
        let d = tempfile::tempdir().unwrap();
        let sk = PrivateKey::generate();
        let store = Store::parse(&format!("dir:{}", d.path().join("store").display())).unwrap();
        let (tests_zip, _) = {
            let src = d.path().join("src");
            std::fs::create_dir_all(src.join("tests")).unwrap();
            std::fs::write(src.join("tests/a.spec.ts"), "test").unwrap();
            crate::taskset_cmd::zip_paths(&src, &["tests".into()]).unwrap()
        };
        let tests_blob = store.put_sealed(&sk.public(), &tests_zip).await.unwrap();
        let ts: TaskSet = serde_json::from_value(serde_json::json!({
            "schema": 1, "name": "demo", "scorer": {"name": "stub"}, "total_time_limit_s": 100,
            "stages": [
              {"id": "stage-1", "inputs_blob": tests_blob, "tests_blob": tests_blob, "output": "web-app", "time_limit_s": 50, "expected_total": 2},
              {"id": "stage-2", "inputs_blob": tests_blob, "tests_blob": tests_blob, "output": "web-app", "time_limit_s": 50, "expected_total": 2}
            ]
        }))
        .unwrap();
        // Stub scorer: passes 1 of 2 iff the tests and the artifact are there.
        let scorer = d.path().join("score.sh");
        std::fs::write(
            &scorer,
            r#"while [ $# -gt 0 ]; do case "$1" in --artifact) A=$2;; --tests) T=$2;; --out) O=$2;; esac; shift 2; done
[ -f "$T/tests/a.spec.ts" ] && grep -q PK-app "$A" || exit 1
printf '{"visibility":"hidden","status":"failed","passed":1,"total":2,"detail":"1/2 tests failed"}' > "$O""#,
        )
        .unwrap();
        let results = d.path().join("results");
        let s1 = results.join("1/stage-1");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::write(
            s1.join("checkpoint.sealed"),
            crucible_crypto::seal(&sk.public(), b"PK-app").unwrap(),
        )
        .unwrap();
        // Stage 2 ran but left only logs.
        std::fs::create_dir_all(results.join("1/stage-2")).unwrap();
        let out = d.path().join("scores");
        let done = score(&ScoreOpts {
            taskset: &ts,
            stages: vec![0, 1],
            results: &results,
            out: &out,
            tests: TestsFrom::Store(&store),
            keys: std::slice::from_ref(&sk),
            scorer: &scorer,
            scrub_env: &[],
        })
        .await
        .unwrap();
        assert_eq!(done.len(), 2);
        let s1 = read_score(&out, 1, "stage-1").unwrap().unwrap();
        assert_eq!(
            (s1.score, s1.max, s1.passed),
            (Some(1.0), Some(2.0), Some(false))
        );
        let s2 = read_score(&out, 1, "stage-2").unwrap().unwrap();
        assert_eq!(
            (s2.status.is_scored(), s2.score, s2.max),
            (true, Some(0.0), Some(2.0))
        );
        assert!(read_score(&out, 2, "stage-1").unwrap().is_none());

        // Hand off to a machine without the platform key: same scores with
        // only the one-run key, and the scorer never sees the key variable.
        let hand = d.path().join("handoff");
        let run_key = handoff(
            &ts,
            &[0, 1],
            &results,
            &store,
            std::slice::from_ref(&sk),
            &hand,
        )
        .await
        .unwrap();
        assert!(hand.join("tests/stage-1.sealed").is_file());
        assert!(hand.join("results/1/stage-2").is_dir());
        assert!(!hand.join("results/1/stage-2/checkpoint.sealed").exists());
        assert!(
            crucible_crypto::open(
                std::slice::from_ref(&sk),
                &std::fs::read(hand.join("tests/stage-1.sealed")).unwrap()
            )
            .is_err()
        );
        std::fs::write(
            &scorer,
            format!(
                "[ -z \"${{PATH_CRUCIBLE_TEST_KEY:-}}\" ] || exit 1\n{}",
                std::fs::read_to_string(&scorer).unwrap()
            ),
        )
        .unwrap();
        let out2 = d.path().join("scores2");
        let keys = [run_key];
        // SAFETY: test-only; nothing else reads this variable.
        unsafe { std::env::set_var("PATH_CRUCIBLE_TEST_KEY", "x") };
        score(&ScoreOpts {
            taskset: &ts,
            stages: vec![0, 1],
            results: &hand.join("results"),
            out: &out2,
            tests: TestsFrom::Dir(&hand.join("tests")),
            keys: &keys,
            scorer: &scorer,
            scrub_env: &["PATH_CRUCIBLE_TEST_KEY".into()],
        })
        .await
        .unwrap();
        let s1 = read_score(&out2, 1, "stage-1").unwrap().unwrap();
        assert_eq!((s1.score, s1.max), (Some(1.0), Some(2.0)));
        let s2 = read_score(&out2, 1, "stage-2").unwrap().unwrap();
        assert_eq!((s2.score, s2.max), (Some(0.0), Some(2.0)));
    }
}
