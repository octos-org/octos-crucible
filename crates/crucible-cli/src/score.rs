//! `crucible score`: score each stage checkpoint with the taskset's scorer.
//!
//! Input layout (same as the publish job's): `<results>/<replica>/<stage>/
//! checkpoint.sealed` (plus `logs.sealed` when the stage ran). Per stage the
//! tests blob is fetched and opened into a temporary directory that is
//! deleted as soon as that stage is scored; each checkpoint is opened into a
//! temporary file the same way. The scorer (`scorers/<name>/score.sh`, see
//! docs/scorer-contract.md) runs with `--visibility hidden`, so its
//! result.json holds only status / passed / total and a fixed detail text:
//! nothing of the tests leaves this job.
//!
//! Output: `<out>/<replica>/<stage>/score.json` (a `ScoreResult`), already
//! normalised for the manifest:
//! - scored with no test results (build failed, app never ready): total =
//!   the taskset's `expected_total`, passed = 0;
//! - a total that differs from `expected_total`: `system_error` (flagged,
//!   not summed);
//! - `system_error` / `rejected`: 0 / 0, so they never count.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use crucible_core::taskset::Stage;
use crucible_core::{ScoreResult, ScoreStatus, TaskSet};
use crucible_crypto::PrivateKey;

use crate::keys::Store;
use crate::zipdir::{self, ExtractLimits};

/// Limits for unpacking a tests blob.
pub const TESTS_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 5_000,
    max_bytes: 256 << 20,
};

pub struct ScoreOpts<'a> {
    pub taskset: &'a TaskSet,
    /// Indices into `taskset.stages` to score.
    pub stages: Vec<usize>,
    pub results: &'a Path,
    pub out: &'a Path,
    pub store: &'a Store,
    pub keys: &'a [PrivateKey],
    /// `score.sh` of the taskset's scorer.
    pub scorer: &'a Path,
}

/// Normalise a scorer result against the stage's declared total.
pub fn normalise(mut r: ScoreResult, expected_total: Option<u32>) -> ScoreResult {
    r.tests = None;
    if !r.status.is_scored() {
        r.passed = 0;
        r.total = 0;
        return r;
    }
    match expected_total {
        Some(e) if r.total == 0 => {
            r.passed = 0;
            r.total = e;
        }
        Some(e) if r.total != e => {
            r.detail = format!("scorer reported {} tests, the taskset expects {e}", r.total);
            r.status = ScoreStatus::SystemError;
            r.passed = 0;
            r.total = 0;
        }
        _ => {}
    }
    r
}

fn system_error(detail: impl Into<String>) -> ScoreResult {
    ScoreResult {
        submission_id: None,
        task_id: None,
        visibility: Some("hidden".into()),
        status: ScoreStatus::SystemError,
        passed: 0,
        total: 0,
        detail: detail.into(),
        tests: None,
    }
}

fn failed(detail: impl Into<String>) -> ScoreResult {
    ScoreResult {
        status: ScoreStatus::Failed,
        ..system_error(detail)
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
fn run_scorer(scorer: &Path, artifact: &Path, tests: &Path, stage: &Stage) -> ScoreResult {
    let work = match tempfile::tempdir() {
        Ok(w) => w,
        Err(e) => return system_error(format!("temp dir: {e}")),
    };
    let out = work.path().join("result.json");
    let status = Command::new("bash")
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
        let plain = o.store.get_sealed(&stage.tests_blob, o.keys).await?;
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
                run_scorer(o.scorer, app.path(), tests.path(), stage)
            } else {
                // The stage ran (it left logs) but produced nothing to score.
                failed("the stage left no checkpoint")
            };
            let result = normalise(result, stage.expected_total);
            let dir = o.out.join(r.to_string()).join(&stage.id);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(
                dir.join("score.json"),
                serde_json::to_string_pretty(&result)? + "\n",
            )?;
            eprintln!(
                "r{r} {}: {:?} {}/{}",
                stage.id, result.status, result.passed, result.total
            );
            done.push((r, stage.id.clone(), result));
        }
        drop(tests);
    }
    Ok(done)
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

    fn r(status: ScoreStatus, passed: u32, total: u32) -> ScoreResult {
        ScoreResult {
            status,
            passed,
            total,
            ..system_error("")
        }
    }

    #[test]
    fn normalise_against_expected_total() {
        let n = normalise(r(ScoreStatus::Failed, 27, 30), Some(30));
        assert_eq!((n.status, n.passed, n.total), (ScoreStatus::Failed, 27, 30));
        // Build failed: no test ran, the stage still counts out of 30.
        let n = normalise(r(ScoreStatus::Failed, 0, 0), Some(30));
        assert_eq!((n.passed, n.total), (0, 30));
        // A pack that collected a different number of tests is flagged.
        let n = normalise(r(ScoreStatus::Passed, 29, 29), Some(30));
        assert_eq!(
            (n.status, n.passed, n.total),
            (ScoreStatus::SystemError, 0, 0)
        );
        assert!(n.detail.contains("expects 30"));
        let n = normalise(r(ScoreStatus::SystemError, 3, 4), Some(30));
        assert_eq!((n.passed, n.total), (0, 0));
        let n = normalise(r(ScoreStatus::Passed, 4, 4), None);
        assert_eq!((n.passed, n.total), (4, 4));
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
            store: &store,
            keys: std::slice::from_ref(&sk),
            scorer: &scorer,
        })
        .await
        .unwrap();
        assert_eq!(done.len(), 2);
        let s1 = read_score(&out, 1, "stage-1").unwrap().unwrap();
        assert_eq!(
            (s1.status, s1.passed, s1.total),
            (ScoreStatus::Failed, 1, 2)
        );
        let s2 = read_score(&out, 1, "stage-2").unwrap().unwrap();
        assert_eq!(
            (s2.status, s2.passed, s2.total),
            (ScoreStatus::Failed, 0, 2)
        );
        assert!(read_score(&out, 2, "stage-1").unwrap().is_none());
    }
}
