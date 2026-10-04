//! `crucible taskset pack|validate|inputs`.
//!
//! `pack` turns a source directory (or a user's zip of one) into sealed
//! blobs plus a public `taskset.json`; `validate` runs the same checks
//! without storing anything. Per stage it builds two zips: inputs (what the agent sees
//! in `/req`) and tests (hidden material for the scorer). The two file sets
//! must be disjoint. `inputs` is what a generation job runs: it downloads
//! only the inputs blobs, never the tests.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result, bail};
use crucible_core::BlobRef;
use crucible_core::TaskSet;
use crucible_core::taskset::{
    Aggregate, DEFAULT_RUNNER, Display, InteractiveRef, MAX_TOTAL_TIME_S, ModelDecl, ScorerRef,
    Stage,
};
use crucible_crypto::{PrivateKey, PublicKey};
use serde::Deserialize;

use crate::keys::Store;
use crate::zipdir::{self, Entry, ExtractLimits, ZipStats};

/// `source.json` next to a taskset: how to cut the source tree into blobs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackSource {
    pub schema: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub scorer: ScorerRef,
    /// Defaults for every stage (a stage may set its own).
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(default)]
    pub packager: Option<String>,
    #[serde(default)]
    pub packager_options: Option<serde_json::Value>,
    #[serde(default)]
    pub scorer_options: Option<serde_json::Value>,
    #[serde(default)]
    pub interactive: Option<InteractiveRef>,
    #[serde(default)]
    pub aggregate: Aggregate,
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub model: Option<ModelDecl>,
    pub total_time_limit_s: u64,
    pub stages: Vec<PackStage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackStage {
    pub id: String,
    /// Stage directory, relative to the source root.
    pub dir: String,
    /// Files or directories (relative to `dir`) handed to the agent.
    pub inputs: Vec<String>,
    /// Files or directories (relative to `dir`) only the scorer sees.
    pub tests: Vec<String>,
    /// The packager plugin (`output` in tasksets written before the plugin
    /// registry); default: the taskset's `packager`.
    #[serde(default, alias = "output")]
    pub packager: Option<String>,
    #[serde(default)]
    pub packager_options: Option<serde_json::Value>,
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(default)]
    pub scorer: Option<ScorerRef>,
    #[serde(default)]
    pub scorer_options: Option<serde_json::Value>,
    #[serde(default)]
    pub interactive: Option<InteractiveRef>,
    pub time_limit_s: u64,
    #[serde(default)]
    pub expected_total: Option<u32>,
}

/// Limits for unpacking a user's taskset zip.
pub const SOURCE_ZIP_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 10_000,
    max_bytes: 256 << 20,
};

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

/// A checked source: what `pack` would store, before storing it.
pub struct Prepared {
    pub src: PackSource,
    /// Per stage: (inputs zip, tests zip, inputs file count, tests file count).
    pub zips: Vec<(Vec<u8>, Vec<u8>, usize, usize)>,
}

impl Prepared {
    fn taskset(&self, blobs: Vec<(BlobRef, BlobRef)>) -> TaskSet {
        let src = &self.src;
        TaskSet {
            schema: src.schema,
            name: src.name.clone(),
            title: None,
            description: src.description.clone(),
            scorer: src.scorer.clone(),
            aggregate: src.aggregate.clone(),
            display: src.display.clone(),
            model: src.model.clone(),
            total_time_limit_s: src.total_time_limit_s,
            stages: src
                .stages
                .iter()
                .zip(blobs)
                .map(|(s, (inputs_blob, tests_blob))| Stage {
                    id: s.id.clone(),
                    inputs_blob,
                    tests_blob,
                    packager: s
                        .packager
                        .clone()
                        .or_else(|| src.packager.clone())
                        .unwrap_or_default(),
                    packager_options: s
                        .packager_options
                        .clone()
                        .or_else(|| src.packager_options.clone()),
                    runner: s
                        .runner
                        .clone()
                        .or_else(|| src.runner.clone())
                        .filter(|r| r != DEFAULT_RUNNER),
                    scorer: s.scorer.clone().filter(|sc| *sc != src.scorer),
                    scorer_options: s
                        .scorer_options
                        .clone()
                        .or_else(|| src.scorer_options.clone()),
                    interactive: s.interactive.clone().or_else(|| src.interactive.clone()),
                    time_limit_s: s.time_limit_s,
                    expected_total: s.expected_total,
                })
                .collect(),
        }
    }
}

/// Parse `source.json`, cut every stage into its inputs and tests zips, and
/// run the taskset checks (format, stage ids, plugins in the registry,
/// total time <= the platform limit). `user`: the rules for uploaded
/// tasksets (only plugins marked `user` in the registry).
pub fn prepare(source: &Path, src_dir: &Path, user: bool) -> Result<Prepared> {
    let raw =
        std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let src: PackSource =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", source.display()))?;
    if let Some(s) = src
        .stages
        .iter()
        .find(|s| s.packager.is_none() && src.packager.is_none())
    {
        bail!("stage {}: no packager (set packager, or output)", s.id);
    }
    let mut zips = Vec::new();
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
        zips.push((inputs_zip, tests_zip, inputs.len(), tests.len()));
    }
    let p = Prepared { src, zips };
    let placeholder = BlobRef {
        sha256: "0".repeat(64),
        key_id: "0".repeat(16),
    };
    let ts = p.taskset(vec![(placeholder.clone(), placeholder); p.zips.len()]);
    ts.validate(MAX_TOTAL_TIME_S)?;
    for s in &ts.stages {
        crate::packagers::check_options(&s.packager, s.packager_options.as_ref())
            .with_context(|| format!("stage {}", s.id))?;
    }
    if user {
        ts.check_user_plugins()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(p)
}

pub async fn pack(
    source: &Path,
    src_dir: &Path,
    user: bool,
    key: &PublicKey,
    store: &Store,
    verbose: bool,
) -> Result<TaskSet> {
    let p = prepare(source, src_dir, user)?;
    let mut blobs = Vec::new();
    for (s, (inputs_zip, tests_zip, ni, nt)) in p.src.stages.iter().zip(&p.zips) {
        let inputs_blob = store.put_sealed(key, inputs_zip).await?;
        let tests_blob = store.put_sealed(key, tests_zip).await?;
        if verbose {
            eprintln!(
                "stage {}: inputs {ni} files -> {}, tests {nt} files -> {}",
                s.id, inputs_blob.sha256, tests_blob.sha256
            );
        }
        blobs.push((inputs_blob, tests_blob));
    }
    let ts = p.taskset(blobs);
    ts.validate(MAX_TOTAL_TIME_S)?;
    Ok(ts)
}

/// Unpack a user's taskset zip into `dest` and return the directory that
/// holds `source.json`: the zip's root, or its only top-level directory
/// (macOS `__MACOSX` / `.DS_Store` entries are ignored).
pub fn unpack_source_zip(zip: &[u8], dest: &Path) -> Result<std::path::PathBuf> {
    zipdir::safe_extract(Cursor::new(zip), dest, SOURCE_ZIP_LIMITS)?;
    source_root(dest)
}

pub fn source_root(dir: &Path) -> Result<std::path::PathBuf> {
    if dir.join("source.json").is_file() {
        return Ok(dir.to_path_buf());
    }
    let entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| !matches!(e.file_name().to_str(), Some("__MACOSX" | ".DS_Store")))
        .collect();
    if let [only] = entries.as_slice()
        && only.file_type()?.is_dir()
        && only.path().join("source.json").is_file()
    {
        return Ok(only.path());
    }
    bail!("no source.json at the top of the taskset (or of its only folder)")
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
        let ts = pack(&source, src.path(), false, &sk.public(), &store, true)
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
            assert!(pack(&source, src.path(), false, &key, &store, true).await.is_err(), "{why}");
        }
    }

    #[test]
    fn validate_refuses_user_mistakes() {
        let src = tree();
        let work = tempfile::tempdir().unwrap();
        let source = work.path().join("source.json");
        let user = true;
        std::fs::write(&source, SOURCE).unwrap();
        let p = prepare(&source, src.path(), user).unwrap();
        assert_eq!(p.zips[0].2, 2); // requirements.yaml + reference/a.png
        assert_eq!(p.zips[0].3, 2); // tests/a.spec.ts + tests/support/e2e.ts
        for (bad, why) in [
            (
                SOURCE.replace("\"playwright\"", "\"arcbench-official\""),
                "scorer not offered to users",
            ),
            (
                SOURCE.replace("\"playwright\"", "\"my-scorer\""),
                "unknown scorer",
            ),
            (
                SOURCE.replacen("\"web-app\"", "\"files\"", 1),
                "playwright needs web-app",
            ),
            (
                SOURCE.replacen("\"output\": \"web-app\"", "\"output\": \"zip\"", 1),
                "unknown packager",
            ),
            (
                SOURCE.replacen(
                    "\"output\": \"web-app\"",
                    "\"output\": \"web-app\", \"packager_options\": {\"require\": [\"x\"]}",
                    1,
                ),
                "web-app takes no options",
            ),
            (
                SOURCE.replacen("\"output\": \"web-app\", ", "", 1),
                "no packager",
            ),
            (
                SOURCE.replace(
                    "\"total_time_limit_s\": 7000",
                    "\"total_time_limit_s\": 18001",
                ),
                "total over 18000 s",
            ),
            (
                SOURCE.replace(
                    "\"total_time_limit_s\": 7000",
                    "\"total_time_limit_s\": 6999",
                ),
                "stages exceed the total",
            ),
            (
                SOURCE.replace("\"id\": \"stage-2\"", "\"id\": \"stage-1\""),
                "duplicate stage",
            ),
            (
                SOURCE.replace("\"id\": \"stage-2\"", "\"id\": \"Stage 2\""),
                "bad stage id",
            ),
            (
                SOURCE.replace("\"schema\": 1", "\"schema\": 1, \"extra\": 1"),
                "unknown field",
            ),
            (
                SOURCE.replace("\"tests\": [\"tests\"]", "\"tests\": [\"/etc\"]"),
                "absolute path",
            ),
            (
                SOURCE.replace("\"tests\": [\"tests\"]", "\"tests\": [\"nope\"]"),
                "missing tests",
            ),
            ("{".into(), "not JSON"),
        ] {
            std::fs::write(&source, &bad).unwrap();
            assert!(prepare(&source, src.path(), user).is_err(), "{why}");
        }
        // Without user rules (built-in tasksets) any registered scorer is
        // accepted, and packager defaults come from the taskset level.
        std::fs::write(
            &source,
            SOURCE
                .replace("\"playwright\"", "\"astro-survey\"")
                .replace("\"output\": \"web-app\", ", "")
                .replace(
                    "\"aggregate\"",
                    "\"packager\": \"files\", \"packager_options\": {\"require\": [\"a\"]}, \"aggregate\"",
                ),
        )
        .unwrap();
        let p = prepare(&source, src.path(), false).unwrap();
        let ts = p.taskset(vec![
            (
                BlobRef {
                    sha256: "0".repeat(64),
                    key_id: "0".repeat(16)
                },
                BlobRef {
                    sha256: "0".repeat(64),
                    key_id: "0".repeat(16)
                }
            );
            2
        ]);
        assert_eq!(ts.stages[1].packager, "files");
        assert!(ts.stages[1].packager_options.is_some());
        std::fs::write(&source, SOURCE.replace("\"playwright\"", "\"nope\"")).unwrap();
        assert!(prepare(&source, src.path(), false).is_err());
    }

    #[test]
    fn finds_source_json_in_a_zip() {
        let src = tree();
        std::fs::write(src.path().join("source.json"), SOURCE).unwrap();
        let entries = |prefix: &str| {
            let mut v = Vec::new();
            let mut st = ZipStats::default();
            zipdir::collect(src.path(), "", &|_| false, &mut v, &mut st).unwrap();
            for e in &mut v {
                e.name = format!("{prefix}{}", e.name);
            }
            v
        };
        for prefix in ["", "my-taskset/"] {
            let zip = zipdir::write_zip(Cursor::new(Vec::new()), &entries(prefix), &[])
                .unwrap()
                .into_inner();
            let d = tempfile::tempdir().unwrap();
            let root = unpack_source_zip(&zip, d.path()).unwrap();
            assert!(prepare(&root.join("source.json"), &root, true).is_ok());
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a")).unwrap();
        std::fs::create_dir_all(d.path().join("b")).unwrap();
        assert!(source_root(d.path()).is_err());
    }
}
