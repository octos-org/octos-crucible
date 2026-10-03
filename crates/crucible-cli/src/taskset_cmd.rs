//! `crucible taskset pack|validate|inputs`.
//!
//! `pack` turns a source directory into sealed blobs plus a public
//! `taskset.json`. Per stage it builds two zips: inputs (what the agent sees
//! in `/req`) and tests (hidden material for the scorer). The two file sets
//! must be disjoint. `inputs` is what a generation job runs: it downloads
//! only the inputs blobs, never the tests.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use crucible_core::TaskSet;
use crucible_core::taskset::{Aggregate, MAX_TOTAL_TIME_S, OutputKind, ScorerRef, Stage};
use crucible_crypto::{PrivateKey, PublicKey};
use serde::Deserialize;

use crate::keys::Store;
use crate::zipdir::{self, Entry, ExtractLimits, ZipStats};

/// `source.json` next to a taskset: how to cut the source tree into blobs.
#[derive(Debug, Deserialize)]
pub struct PackSource {
    pub schema: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub scorer: ScorerRef,
    #[serde(default)]
    pub aggregate: Aggregate,
    pub total_time_limit_s: u64,
    pub stages: Vec<PackStage>,
}

#[derive(Debug, Deserialize)]
pub struct PackStage {
    pub id: String,
    /// Stage directory, relative to the source root.
    pub dir: String,
    /// Files or directories (relative to `dir`) handed to the agent.
    pub inputs: Vec<String>,
    /// Files or directories (relative to `dir`) only the scorer sees.
    pub tests: Vec<String>,
    pub output: OutputKind,
    pub time_limit_s: u64,
    #[serde(default)]
    pub expected_total: Option<u32>,
}

/// Limits for unpacking an inputs blob.
pub const INPUT_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 5_000,
    max_bytes: 512 << 20,
};

fn rel_ok(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Zip the given paths under `root`; error if any is missing or empty.
pub fn zip_paths(root: &Path, paths: &[String]) -> Result<(Vec<u8>, BTreeSet<String>)> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut stats = ZipStats::default();
    for p in paths {
        if !rel_ok(p) {
            bail!("{p:?} must be a relative path without '..'");
        }
        let before = entries.len();
        zipdir::collect(root, p, &|_| false, &mut entries, &mut stats)?;
        if entries.len() == before {
            bail!("{} has no files", root.join(p).display());
        }
    }
    if stats.dropped_links > 0 {
        bail!("{} contains symlinks; refusing to pack", root.display());
    }
    let names: BTreeSet<String> = entries.iter().map(|e| e.name.clone()).collect();
    if names.len() != entries.len() {
        bail!("overlapping paths in {paths:?}");
    }
    let zip = zipdir::write_zip(Cursor::new(Vec::new()), &entries, &[])?.into_inner();
    Ok((zip, names))
}

pub async fn pack(
    source: &Path,
    src_dir: &Path,
    key: &PublicKey,
    store: &Store,
) -> Result<TaskSet> {
    let raw =
        std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let src: PackSource =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", source.display()))?;
    let mut stages = Vec::new();
    for s in &src.stages {
        if !rel_ok(&s.dir) {
            bail!("stage {}: dir {:?} must be relative", s.id, s.dir);
        }
        let root = src_dir.join(&s.dir);
        let (inputs_zip, inputs) =
            zip_paths(&root, &s.inputs).with_context(|| format!("stage {} inputs", s.id))?;
        let (tests_zip, tests) =
            zip_paths(&root, &s.tests).with_context(|| format!("stage {} tests", s.id))?;
        if let Some(both) = inputs.intersection(&tests).next() {
            bail!("stage {}: {both} is both an input and a test file", s.id);
        }
        let inputs_blob = store.put_sealed(key, &inputs_zip).await?;
        let tests_blob = store.put_sealed(key, &tests_zip).await?;
        eprintln!(
            "stage {}: inputs {} files -> {}, tests {} files -> {}",
            s.id,
            inputs.len(),
            inputs_blob.sha256,
            tests.len(),
            tests_blob.sha256
        );
        stages.push(Stage {
            id: s.id.clone(),
            inputs_blob,
            tests_blob,
            output: s.output,
            time_limit_s: s.time_limit_s,
            expected_total: s.expected_total,
        });
    }
    let ts = TaskSet {
        schema: src.schema,
        name: src.name,
        description: src.description,
        scorer: src.scorer,
        aggregate: src.aggregate,
        total_time_limit_s: src.total_time_limit_s,
        stages,
    };
    ts.validate(MAX_TOTAL_TIME_S)?;
    Ok(ts)
}

pub fn load(path: &Path) -> Result<TaskSet> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let ts: TaskSet =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(ts)
}

/// Download and unpack the inputs of the first `n` stages into
/// `out/<stage id>/`. Never touches a tests blob.
pub async fn fetch_inputs(
    ts: &TaskSet,
    n: usize,
    store: &Store,
    keys: &[PrivateKey],
    out: &Path,
) -> Result<()> {
    for s in ts.stages.iter().take(n) {
        let plain = store.get_sealed(&s.inputs_blob, keys).await?;
        let dest = out.join(&s.id);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        let files = zipdir::safe_extract(Cursor::new(plain), &dest, INPUT_LIMITS)
            .with_context(|| format!("stage {} inputs", s.id))?;
        eprintln!("stage {}: {files} input files", s.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for s in ["s1", "s2"] {
            let r = d.path().join(s);
            std::fs::create_dir_all(r.join("reference")).unwrap();
            std::fs::create_dir_all(r.join("tests/support")).unwrap();
            std::fs::write(r.join("requirements.yaml"), format!("id: {s}")).unwrap();
            std::fs::write(r.join("reference/a.png"), "png").unwrap();
            std::fs::write(r.join("tests/a.spec.ts"), "test('x')").unwrap();
            std::fs::write(r.join("tests/support/e2e.ts"), "e2e").unwrap();
        }
        d
    }

    const SOURCE: &str = r#"{
      "schema": 1, "name": "demo", "scorer": {"name": "playwright"},
      "aggregate": "sum", "total_time_limit_s": 7000,
      "stages": [
        {"id": "stage-1", "dir": "s1", "inputs": ["requirements.yaml", "reference"], "tests": ["tests"], "output": "web-app", "time_limit_s": 3000, "expected_total": 1},
        {"id": "stage-2", "dir": "s2", "inputs": ["requirements.yaml", "reference"], "tests": ["tests"], "output": "web-app", "time_limit_s": 4000}
      ]
    }"#;

    #[tokio::test]
    async fn pack_then_fetch_inputs_only() {
        let src = tree();
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("source.json");
        std::fs::write(&source, SOURCE).unwrap();
        let store = Store::parse(&format!("dir:{}", work.path().join("store").display())).unwrap();
        let sk = PrivateKey::generate();
        let ts = pack(&source, src.path(), &sk.public(), &store)
            .await
            .unwrap();
        assert_eq!(ts.stages.len(), 2);
        assert_eq!(ts.stages[0].inputs_blob.key_id, sk.public().key_id());
        // Round trip through JSON as the registered file would.
        let ts: TaskSet = serde_json::from_str(&serde_json::to_string(&ts).unwrap()).unwrap();

        let out = work.path().join("inputs");
        fetch_inputs(&ts, 1, &store, &[sk], &out).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("stage-1/requirements.yaml")).unwrap(),
            "id: s1"
        );
        assert!(out.join("stage-1/reference/a.png").is_file());
        assert!(!out.join("stage-1/tests").exists());
        assert!(!out.join("stage-2").exists());
    }

    #[tokio::test]
    async fn pack_refuses_overlap_missing_and_too_long() {
        let src = tree();
        let work = tempfile::tempdir().unwrap();
        let store = Store::parse(&format!("dir:{}", work.path().join("store").display())).unwrap();
        let key = PrivateKey::generate().public();
        let source = work.path().join("source.json");
        for (bad, why) in [
            (SOURCE.replace(r#""inputs": ["requirements.yaml", "reference"], "tests": ["tests"], "output": "web-app", "time_limit_s": 3000"#, r#""inputs": ["requirements.yaml", "tests/a.spec.ts"], "tests": ["tests"], "output": "web-app", "time_limit_s": 3000"#), "overlap"),
            (SOURCE.replace("\"reference\"], \"tests\": [\"tests\"], \"output\": \"web-app\", \"time_limit_s\": 4000", "\"nope\"], \"tests\": [\"tests\"], \"output\": \"web-app\", \"time_limit_s\": 4000"), "missing"),
            (SOURCE.replace("\"total_time_limit_s\": 7000", "\"total_time_limit_s\": 18001"), "too long"),
            (SOURCE.replace("\"dir\": \"s1\"", "\"dir\": \"../s1\""), "escape"),
        ] {
            assert_ne!(bad, SOURCE, "{why}");
            std::fs::write(&source, bad).unwrap();
            assert!(pack(&source, src.path(), &key, &store).await.is_err(), "{why}");
        }
    }
}
